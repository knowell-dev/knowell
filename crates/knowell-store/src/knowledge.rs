//! Knowledge (memory) records: the current row, immutable content versions
//! with their evidence, and the append-only history of state transitions.
//!
//! The domain model (state machine, acceptance policy, staleness and
//! conflict rules, secret scanning) lives in `knowell-knowledge`; this module
//! persists its results as plain rows. Actors (`author`, history `actor`)
//! are jsonb in whatever form the engine serialises them.
//!
//! - [`insert_record`] writes a record with its first stored version,
//!   evidence and history.
//! - [`update_record`] applies a change under optimistic concurrency: the
//!   caller names the content `version` and the row `revision` it read and
//!   gets [`StoreError::Conflict`] when either moved. A content change
//!   ([`RecordContent`]) appends version `n + 1`; earlier versions and their
//!   evidence are never changed.
//! - [`list_records`] (scope, state, kind, subject, tag filters, keyset
//!   pages) and [`search_records`] (PostgreSQL full text, `simple`
//!   configuration; its rank is `ts_rank_cd`, not BM25).
//! - [`records_citing_files`] and [`records_about_symbols`] find the records a
//!   change set may have made stale.
//! - [`record_versions`] and [`record_history`] for audit and resume.

use std::collections::BTreeMap;

use knowell_core::{ContentHash, LineRange, RepoPath};
use sqlx::{Connection, PgConnection};
use time::OffsetDateTime;
use uuid::Uuid;

use crate::error::{StoreError, Violation, violation};
use crate::hierarchy::stored_path;
use crate::ids::{KnowledgeRecordId, OrganizationId, ProjectId, TaskId, WorkspaceId};
use crate::types::{
    KnowledgeAction, KnowledgeKind, KnowledgeScopeKind, KnowledgeState, check_label, check_text,
    from_i32, from_revision, hash_bytes, hash_from_bytes, to_i32, to_revision,
};

/// Longest title, in bytes.
pub const MAX_TITLE_BYTES: usize = 1000;
/// Longest body, in bytes.
pub const MAX_BODY_BYTES: usize = 256 * 1024;
/// Longest subject key, in bytes.
pub const MAX_SUBJECT_BYTES: usize = 128;
/// Longest history reason, in bytes.
pub const MAX_REASON_BYTES: usize = 4096;
/// Longest user key, view key, tag or symbol id, in bytes.
pub const MAX_LABEL_BYTES: usize = 256;
/// Most evidence entries per version.
pub const MAX_EVIDENCE: usize = 1024;
/// Most records one listing or search returns.
pub const MAX_RECORDS_LISTED: u32 = 1000;
/// Longest full-text query, in bytes.
pub const MAX_QUERY_BYTES: usize = 1000;

/// Where a record applies: the store form of `knowell_knowledge::Scope`,
/// with workspaces, projects and tasks referenced by id.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum RecordScope {
    /// The whole organization.
    Organization,
    /// One workspace.
    Workspace(WorkspaceId),
    /// One project (its workspace is looked up).
    Project(ProjectId),
    /// Private to one task.
    Task(TaskId),
    /// Private to one user, by the engine's user key (1-256 bytes, no
    /// control characters).
    User(String),
}

impl RecordScope {
    /// The scope's kind.
    pub fn kind(&self) -> KnowledgeScopeKind {
        match self {
            Self::Organization => KnowledgeScopeKind::Organization,
            Self::Workspace(_) => KnowledgeScopeKind::Workspace,
            Self::Project(_) => KnowledgeScopeKind::Project,
            Self::Task(_) => KnowledgeScopeKind::Task,
            Self::User(_) => KnowledgeScopeKind::User,
        }
    }

    /// Canonical text form, as in the `scope_key` column: `org`,
    /// `workspace:<uuid>`, `project:<uuid>`, `task:<uuid>`, `user:<key>`.
    pub fn key(&self) -> String {
        match self {
            Self::Organization => "org".to_owned(),
            Self::Workspace(id) => format!("workspace:{id}"),
            Self::Project(id) => format!("project:{id}"),
            Self::Task(id) => format!("task:{id}"),
            Self::User(key) => format!("user:{key}"),
        }
    }

    fn validate(&self) -> Result<(), StoreError> {
        match self {
            Self::User(key) => check_label("user key", key, MAX_LABEL_BYTES),
            _ => Ok(()),
        }
    }
}

/// A pointer to the code a record version is based on.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct RecordEvidence {
    /// The project the file belongs to (same organization as the record).
    pub project: ProjectId,
    /// The view the evidence was read from, as the engine names it
    /// (1-256 bytes).
    pub view: String,
    /// The commit of that view: 7 to 64 lowercase hex digits.
    pub commit: String,
    /// File path relative to the project root.
    pub path: RepoPath,
    /// 1-based inclusive line span.
    pub lines: LineRange,
    /// Hash of the file content the span was read from.
    pub content_hash: ContentHash,
}

/// One state transition of a record (input and output).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HistoryEntry {
    /// When it happened.
    pub at: OffsetDateTime,
    /// Who did it (the engine's actor serialisation).
    pub actor: serde_json::Value,
    /// What was done.
    pub action: KnowledgeAction,
    /// State before; `None` for the creation entry.
    pub from: Option<KnowledgeState>,
    /// State after.
    pub to: KnowledgeState,
    /// Content version after the action (at least 1).
    pub version: u32,
    /// Why (1-4096 bytes; the domain checks it for secrets).
    pub reason: String,
}

/// A record to create.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewRecord {
    /// Identifier chosen by the caller.
    pub id: KnowledgeRecordId,
    /// Owning organization.
    pub organization: OrganizationId,
    /// Where the record applies (inside `organization`).
    pub scope: RecordScope,
    /// How the record came to exist.
    pub kind: KnowledgeKind,
    /// Normalised subject key: lowercase segments of `[a-z0-9_-]` joined by
    /// `.`, at most 128 bytes.
    pub subject: String,
    /// Title (1-1000 bytes).
    pub title: String,
    /// Markdown body (at most 256 KiB).
    pub body: String,
    /// Lifecycle state.
    pub state: KnowledgeState,
    /// Content version (at least 1); this version's content is stored.
    pub version: u32,
    /// Who created it.
    pub author: serde_json::Value,
    /// Pinned into every bootstrap pack.
    pub pinned: bool,
    /// Tags (each 1-256 bytes).
    pub tags: Vec<String>,
    /// Symbol ids the record is about (each 1-256 bytes).
    pub related_symbols: Vec<String>,
    /// The record that replaced this one (same organization).
    pub superseded_by: Option<KnowledgeRecordId>,
    /// Evidence of this version.
    pub evidence: Vec<RecordEvidence>,
    /// History so far, oldest first.
    pub history: Vec<HistoryEntry>,
    /// Creation time.
    pub created_at: OffsetDateTime,
    /// Time of the last change; also when this version became current.
    pub updated_at: OffsetDateTime,
}

/// New content for [`RecordUpdate::content`]: stored as the next version.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecordContent {
    /// Title (1-1000 bytes).
    pub title: String,
    /// Markdown body (at most 256 KiB).
    pub body: String,
    /// Tags.
    pub tags: Vec<String>,
    /// Evidence of the new version (replaces the old set).
    pub evidence: Vec<RecordEvidence>,
}

/// A change to an existing record, applied by [`update_record`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecordUpdate {
    /// The record.
    pub id: KnowledgeRecordId,
    /// The content version the change is based on.
    pub expected_version: u32,
    /// The row revision the change is based on ([`StoredRecord::revision`]).
    pub expected_revision: u64,
    /// New state.
    pub state: KnowledgeState,
    /// New pin flag.
    pub pinned: bool,
    /// New related symbols.
    pub related_symbols: Vec<String>,
    /// New superseding record.
    pub superseded_by: Option<KnowledgeRecordId>,
    /// New content; `Some` stores version `expected_version + 1`, `None`
    /// keeps the content and the version.
    pub content: Option<RecordContent>,
    /// History entries to append, oldest first.
    pub history: Vec<HistoryEntry>,
    /// Time of this change.
    pub updated_at: OffsetDateTime,
}

/// A stored record with the evidence of its current version.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoredRecord {
    /// Identifier.
    pub id: KnowledgeRecordId,
    /// Owning organization.
    pub organization: OrganizationId,
    /// Where the record applies.
    pub scope: RecordScope,
    /// The workspace of a workspace- or project-scoped record.
    pub workspace: Option<WorkspaceId>,
    /// How the record came to exist.
    pub kind: KnowledgeKind,
    /// Subject key.
    pub subject: String,
    /// Title of the current version.
    pub title: String,
    /// Body of the current version.
    pub body: String,
    /// Lifecycle state.
    pub state: KnowledgeState,
    /// Content version.
    pub version: u32,
    /// Row revision: the optimistic-concurrency token, bumped by every
    /// update.
    pub revision: u64,
    /// Who created it.
    pub author: serde_json::Value,
    /// Pinned into every bootstrap pack.
    pub pinned: bool,
    /// Tags of the current version.
    pub tags: Vec<String>,
    /// Related symbol ids.
    pub related_symbols: Vec<String>,
    /// The record that replaced this one.
    pub superseded_by: Option<KnowledgeRecordId>,
    /// Evidence of the current version, in the order it was given.
    pub evidence: Vec<RecordEvidence>,
    /// Creation time.
    pub created_at: OffsetDateTime,
    /// Time of the last change.
    pub updated_at: OffsetDateTime,
}

/// One stored content version (including the current one).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecordVersion {
    /// Version number.
    pub version: u32,
    /// Title at that version.
    pub title: String,
    /// Body at that version.
    pub body: String,
    /// Tags at that version.
    pub tags: Vec<String>,
    /// Evidence at that version.
    pub evidence: Vec<RecordEvidence>,
    /// When it became current.
    pub created_at: OffsetDateTime,
    /// When the next version replaced it; `None` for the newest version.
    pub replaced_at: Option<OffsetDateTime>,
}

/// Keyset position for paging through [`list_records`]: the listing
/// continues with records updated earlier than this one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RecordCursor {
    /// `updated_at` of the last record seen.
    pub updated_at: OffsetDateTime,
    /// Id of the last record seen (tie-break).
    pub id: KnowledgeRecordId,
}

impl RecordCursor {
    /// The cursor after `record` (pass the last record of a page). A record
    /// updated while paging may move to an earlier page.
    pub fn after(record: &StoredRecord) -> Self {
        Self {
            updated_at: record.updated_at,
            id: record.id,
        }
    }
}

/// What [`list_records`] and [`search_records`] return.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecordFilter {
    /// The organization (required: tenants never mix).
    pub organization: OrganizationId,
    /// Records in exactly one of these scopes; empty = every scope.
    pub scopes: Vec<RecordScope>,
    /// Records in one of these states; empty = every state.
    pub states: Vec<KnowledgeState>,
    /// Records of one of these kinds; empty = every kind.
    pub kinds: Vec<KnowledgeKind>,
    /// Records with exactly this subject key.
    pub subject: Option<String>,
    /// Only pinned (`Some(true)`) or unpinned (`Some(false)`) records.
    pub pinned: Option<bool>,
    /// Records carrying this tag.
    pub tag: Option<String>,
    /// Most records to return, 1..=[`MAX_RECORDS_LISTED`].
    pub limit: u32,
    /// Continue after this position ([`list_records`] only).
    pub before: Option<RecordCursor>,
}

impl RecordFilter {
    /// Every record of `organization`, at most `limit`.
    pub fn new(organization: OrganizationId, limit: u32) -> Self {
        Self {
            organization,
            scopes: Vec::new(),
            states: Vec::new(),
            kinds: Vec::new(),
            subject: None,
            pinned: None,
            tag: None,
            limit,
            before: None,
        }
    }
}

/// A full-text match.
#[derive(Debug, Clone, PartialEq)]
pub struct RecordHit {
    /// The record.
    pub record: StoredRecord,
    /// PostgreSQL `ts_rank_cd` (title and subject weigh more than the body).
    /// Comparable only within one search; not a BM25 score.
    pub rank: f32,
}

/// A file whose content changed, for [`records_citing_files`].
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct EvidenceChange {
    /// The project.
    pub project: ProjectId,
    /// The file.
    pub path: RepoPath,
    /// The content hash it had before the change.
    pub old_hash: ContentHash,
}

#[derive(sqlx::FromRow)]
struct RecordRow {
    id: KnowledgeRecordId,
    organization_id: OrganizationId,
    scope_kind: KnowledgeScopeKind,
    workspace_id: Option<WorkspaceId>,
    project_id: Option<ProjectId>,
    task_id: Option<TaskId>,
    user_key: Option<String>,
    kind: KnowledgeKind,
    subject: String,
    title: String,
    body: String,
    state: KnowledgeState,
    version: i32,
    revision: i64,
    author: serde_json::Value,
    pinned: bool,
    tags: Vec<String>,
    related_symbols: Vec<String>,
    superseded_by: Option<KnowledgeRecordId>,
    created_at: OffsetDateTime,
    updated_at: OffsetDateTime,
}

#[derive(sqlx::FromRow)]
struct HitRow {
    #[sqlx(flatten)]
    record: RecordRow,
    rank: f32,
}

impl RecordRow {
    fn scope(&self) -> Result<RecordScope, StoreError> {
        let corrupt = || {
            StoreError::Corrupt(format!(
                "knowledge record {} has inconsistent scope columns",
                self.id
            ))
        };
        Ok(match self.scope_kind {
            KnowledgeScopeKind::Organization => RecordScope::Organization,
            KnowledgeScopeKind::Workspace => {
                RecordScope::Workspace(self.workspace_id.ok_or_else(corrupt)?)
            }
            KnowledgeScopeKind::Project => {
                RecordScope::Project(self.project_id.ok_or_else(corrupt)?)
            }
            KnowledgeScopeKind::Task => RecordScope::Task(self.task_id.ok_or_else(corrupt)?),
            KnowledgeScopeKind::User => {
                RecordScope::User(self.user_key.clone().ok_or_else(corrupt)?)
            }
        })
    }

    fn into_record(self, evidence: Vec<RecordEvidence>) -> Result<StoredRecord, StoreError> {
        let scope = self.scope()?;
        Ok(StoredRecord {
            id: self.id,
            organization: self.organization_id,
            scope,
            workspace: self.workspace_id,
            kind: self.kind,
            subject: self.subject,
            title: self.title,
            body: self.body,
            state: self.state,
            version: from_i32(self.version, "record version")?,
            revision: from_revision(self.revision)?,
            author: self.author,
            pinned: self.pinned,
            tags: self.tags,
            related_symbols: self.related_symbols,
            superseded_by: self.superseded_by,
            evidence,
            created_at: self.created_at,
            updated_at: self.updated_at,
        })
    }
}

#[derive(sqlx::FromRow)]
struct EvidenceRow {
    record_id: KnowledgeRecordId,
    version: i32,
    project_id: ProjectId,
    view_key: String,
    commit_id: String,
    path: String,
    start_line: i32,
    end_line: i32,
    content_hash: Vec<u8>,
}

impl EvidenceRow {
    fn into_evidence(self) -> Result<RecordEvidence, StoreError> {
        let lines = LineRange::new(
            from_i32(self.start_line, "evidence start line")?,
            from_i32(self.end_line, "evidence end line")?,
        )
        .map_err(|e| StoreError::Corrupt(format!("stored evidence lines: {e}")))?;
        Ok(RecordEvidence {
            project: self.project_id,
            view: self.view_key,
            commit: self.commit_id,
            path: stored_path(self.path)?,
            lines,
            content_hash: hash_from_bytes(&self.content_hash)?,
        })
    }
}

#[derive(sqlx::FromRow)]
struct HistoryRow {
    at: OffsetDateTime,
    actor: serde_json::Value,
    action: KnowledgeAction,
    from_state: Option<KnowledgeState>,
    to_state: KnowledgeState,
    version: i32,
    reason: String,
}

#[derive(sqlx::FromRow)]
struct VersionRow {
    version: i32,
    title: String,
    body: String,
    tags: Vec<String>,
    created_at: OffsetDateTime,
}

macro_rules! record_columns {
    () => {
        "id, organization_id, scope_kind, workspace_id, project_id, task_id, user_key, kind,
         subject, title, body, state, version, revision, author, pinned, tags, related_symbols,
         superseded_by, created_at, updated_at"
    };
}

/// The filter conditions of [`RecordFilter`], parameters `$1`..`$7`.
macro_rules! record_filter {
    () => {
        "organization_id = $1
           AND (cardinality($2::text[]) = 0 OR scope_key = ANY($2))
           AND (cardinality($3::knowledge_state[]) = 0 OR state = ANY($3))
           AND (cardinality($4::knowledge_kind[]) = 0 OR kind = ANY($4))
           AND ($5::text IS NULL OR subject = $5)
           AND ($6::boolean IS NULL OR pinned = $6)
           AND ($7::text IS NULL OR $7 = ANY(tags))"
    };
}

/// Checks a subject key: segments of `[a-z0-9_-]` joined by single dots.
fn check_subject(subject: &str) -> Result<(), StoreError> {
    let well_formed = !subject.is_empty()
        && subject.len() <= MAX_SUBJECT_BYTES
        && subject.split('.').all(|segment| {
            !segment.is_empty()
                && segment
                    .bytes()
                    .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_' || b == b'-')
        });
    if well_formed {
        Ok(())
    } else {
        Err(StoreError::invalid(
            "subject must be 1-128 bytes of lowercase [a-z0-9_-] segments joined by `.`",
        ))
    }
}

fn check_commit(commit: &str) -> Result<(), StoreError> {
    let ok = (7..=64).contains(&commit.len())
        && commit
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b));
    if ok {
        Ok(())
    } else {
        Err(StoreError::invalid(
            "evidence commit must be 7 to 64 lowercase hex digits",
        ))
    }
}

fn check_labels(field: &str, labels: &[String]) -> Result<(), StoreError> {
    labels
        .iter()
        .try_for_each(|label| check_label(field, label, MAX_LABEL_BYTES))
}

fn check_content(title: &str, body: &str, tags: &[String]) -> Result<(), StoreError> {
    check_text("title", title, MAX_TITLE_BYTES, false)?;
    check_text("body", body, MAX_BODY_BYTES, true)?;
    check_labels("tag", tags)
}

fn check_evidence(evidence: &[RecordEvidence]) -> Result<(), StoreError> {
    if evidence.len() > MAX_EVIDENCE {
        return Err(StoreError::invalid(format!(
            "a record version holds at most {MAX_EVIDENCE} evidence entries"
        )));
    }
    for e in evidence {
        check_label("evidence view", &e.view, MAX_LABEL_BYTES)?;
        check_commit(&e.commit)?;
        to_i32(e.lines.end(), "evidence end line")?;
    }
    Ok(())
}

fn check_history(history: &[HistoryEntry]) -> Result<(), StoreError> {
    for entry in history {
        check_text("history reason", &entry.reason, MAX_REASON_BYTES, false)?;
        if entry.version == 0 {
            return Err(StoreError::invalid("history version must be at least 1"));
        }
        to_i32(entry.version, "history version")?;
    }
    Ok(())
}

fn check_limit(limit: u32) -> Result<i64, StoreError> {
    if limit == 0 || limit > MAX_RECORDS_LISTED {
        return Err(StoreError::invalid(format!(
            "record listing limit must be between 1 and {MAX_RECORDS_LISTED}"
        )));
    }
    Ok(i64::from(limit))
}

fn check_version(version: u32) -> Result<i32, StoreError> {
    if version == 0 {
        return Err(StoreError::invalid("record version must be at least 1"));
    }
    to_i32(version, "record version")
}

/// Scope columns of a new record: (workspace, project, task, user key).
type ScopeColumns = (
    Option<WorkspaceId>,
    Option<ProjectId>,
    Option<TaskId>,
    Option<String>,
);

async fn scope_columns(
    conn: &mut PgConnection,
    organization: OrganizationId,
    scope: &RecordScope,
) -> Result<ScopeColumns, StoreError> {
    scope.validate()?;
    Ok(match scope {
        RecordScope::Organization => (None, None, None, None),
        RecordScope::Workspace(ws) => (Some(*ws), None, None, None),
        RecordScope::Project(project) => {
            let row: Option<(WorkspaceId, OrganizationId)> =
                sqlx::query_as("SELECT workspace_id, organization_id FROM project WHERE id = $1")
                    .bind(project)
                    .fetch_optional(&mut *conn)
                    .await?;
            match row {
                Some((ws, org)) if org == organization => (Some(ws), Some(*project), None, None),
                // Another tenant's project is reported like a missing one.
                _ => return Err(StoreError::not_found("project", project)),
            }
        }
        RecordScope::Task(task) => (None, None, Some(*task), None),
        RecordScope::User(key) => (None, None, None, Some(key.clone())),
    })
}

/// Maps a failed record write to a precise error by constraint name.
fn record_write_error(
    err: sqlx::Error,
    id: KnowledgeRecordId,
    organization: OrganizationId,
    scope: &RecordScope,
    superseded_by: Option<KnowledgeRecordId>,
) -> StoreError {
    let constraint = |c: &Option<String>| c.clone().unwrap_or_default();
    match violation(&err) {
        Some(Violation::Unique(_)) => StoreError::already_exists("knowledge record", id),
        Some(Violation::ForeignKey(c)) => match constraint(&c).as_str() {
            "knowledge_record_superseded_by_fk" => StoreError::not_found(
                "superseding knowledge record in this organization",
                superseded_by.map(|s| s.to_string()).unwrap_or_default(),
            ),
            "knowledge_record_organization_id_fkey" => {
                StoreError::not_found("organization", organization)
            }
            _ => match scope {
                RecordScope::Workspace(ws) => {
                    StoreError::not_found("workspace in this organization", ws)
                }
                RecordScope::Project(p) => StoreError::not_found("project", p),
                RecordScope::Task(t) => StoreError::not_found("task in this organization", t),
                _ => StoreError::Database(err),
            },
        },
        Some(Violation::Check(c)) => {
            StoreError::invalid(format!("knowledge record violates `{}`", constraint(&c)))
        }
        None => StoreError::Database(err),
    }
}

async fn insert_version(
    conn: &mut PgConnection,
    id: KnowledgeRecordId,
    version: i32,
    content: (&str, &str, &[String]),
    at: OffsetDateTime,
) -> Result<(), StoreError> {
    let (title, body, tags) = content;
    sqlx::query(
        "INSERT INTO knowledge_record_version (record_id, version, title, body, tags, created_at)
         VALUES ($1, $2, $3, $4, $5, $6)",
    )
    .bind(id)
    .bind(version)
    .bind(title)
    .bind(body)
    .bind(tags)
    .bind(at)
    .execute(conn)
    .await
    .map_err(|e| match violation(&e) {
        Some(Violation::Unique(_)) => StoreError::Corrupt(format!(
            "version {version} of knowledge record {id} already exists"
        )),
        _ => StoreError::Database(e),
    })?;
    Ok(())
}

async fn insert_evidence(
    conn: &mut PgConnection,
    organization: OrganizationId,
    id: KnowledgeRecordId,
    version: i32,
    evidence: &[RecordEvidence],
) -> Result<(), StoreError> {
    if evidence.is_empty() {
        return Ok(());
    }
    let n = evidence.len();
    let (mut projects, mut views, mut commits, mut paths) = (
        Vec::with_capacity(n),
        Vec::with_capacity(n),
        Vec::with_capacity(n),
        Vec::with_capacity(n),
    );
    let (mut starts, mut ends, mut hashes) = (
        Vec::with_capacity(n),
        Vec::with_capacity(n),
        Vec::with_capacity(n),
    );
    for e in evidence {
        projects.push(e.project);
        views.push(e.view.as_str());
        commits.push(e.commit.as_str());
        paths.push(e.path.as_str());
        starts.push(to_i32(e.lines.start(), "evidence start line")?);
        ends.push(to_i32(e.lines.end(), "evidence end line")?);
        hashes.push(hash_bytes(&e.content_hash));
    }
    // Joining `project` on the organization keeps evidence inside the tenant:
    // a foreign project yields fewer rows than given.
    let written = sqlx::query(
        "INSERT INTO knowledge_evidence (record_id, version, ordinal, project_id, view_key,
                                         commit_id, path, start_line, end_line, content_hash)
         SELECT $1, $2, (u.ord - 1)::integer, u.project_id, u.view_key, u.commit_id, u.path,
                u.start_line, u.end_line, u.content_hash
         FROM unnest($3::uuid[], $4::text[], $5::text[], $6::text[], $7::integer[],
                     $8::integer[], $9::bytea[])
              WITH ORDINALITY AS u(project_id, view_key, commit_id, path, start_line, end_line,
                                   content_hash, ord)
         JOIN project p ON p.id = u.project_id AND p.organization_id = $10",
    )
    .bind(id)
    .bind(version)
    .bind(&projects)
    .bind(&views)
    .bind(&commits)
    .bind(&paths)
    .bind(&starts)
    .bind(&ends)
    .bind(&hashes)
    .bind(organization)
    .execute(conn)
    .await?
    .rows_affected();
    if usize::try_from(written).ok() != Some(n) {
        return Err(StoreError::not_found(
            "evidence project in this organization",
            "one or more evidence entries",
        ));
    }
    Ok(())
}

async fn insert_history(
    conn: &mut PgConnection,
    id: KnowledgeRecordId,
    history: &[HistoryEntry],
) -> Result<(), StoreError> {
    if history.is_empty() {
        return Ok(());
    }
    let n = history.len();
    let (mut ats, mut actors, mut actions, mut froms) = (
        Vec::with_capacity(n),
        Vec::with_capacity(n),
        Vec::with_capacity(n),
        Vec::with_capacity(n),
    );
    let (mut tos, mut versions, mut reasons) = (
        Vec::with_capacity(n),
        Vec::with_capacity(n),
        Vec::with_capacity(n),
    );
    for h in history {
        ats.push(h.at);
        actors.push(h.actor.clone());
        actions.push(h.action);
        froms.push(h.from);
        tos.push(h.to);
        versions.push(to_i32(h.version, "history version")?);
        reasons.push(h.reason.as_str());
    }
    // Identity values follow the ORDER BY, so ids keep the given order.
    sqlx::query(
        "INSERT INTO knowledge_history (record_id, at, actor, action, from_state, to_state,
                                        version, reason)
         SELECT $1, u.at, u.actor, u.action, u.from_state, u.to_state, u.version, u.reason
         FROM unnest($2::timestamptz[], $3::jsonb[], $4::knowledge_action[],
                     $5::knowledge_state[], $6::knowledge_state[], $7::integer[], $8::text[])
              WITH ORDINALITY AS u(at, actor, action, from_state, to_state, version, reason, ord)
         ORDER BY u.ord",
    )
    .bind(id)
    .bind(&ats)
    .bind(&actors)
    .bind(&actions)
    .bind(&froms)
    .bind(&tos)
    .bind(&versions)
    .bind(&reasons)
    .execute(conn)
    .await?;
    Ok(())
}

/// Loads the evidence of the given (record, version) pairs, grouped by
/// record in stored order.
async fn evidence_of(
    conn: &mut PgConnection,
    pairs: &[(KnowledgeRecordId, i32)],
) -> Result<BTreeMap<KnowledgeRecordId, Vec<RecordEvidence>>, StoreError> {
    let mut out: BTreeMap<KnowledgeRecordId, Vec<RecordEvidence>> = BTreeMap::new();
    if pairs.is_empty() {
        return Ok(out);
    }
    let (ids, versions): (Vec<KnowledgeRecordId>, Vec<i32>) = pairs.iter().copied().unzip();
    let rows = sqlx::query_as::<_, EvidenceRow>(
        "SELECT e.record_id, e.version, e.project_id, e.view_key, e.commit_id, e.path,
                e.start_line, e.end_line, e.content_hash
         FROM knowledge_evidence e
         JOIN unnest($1::uuid[], $2::integer[]) AS p(record_id, version)
           ON e.record_id = p.record_id AND e.version = p.version
         ORDER BY e.record_id, e.version, e.ordinal",
    )
    .bind(&ids)
    .bind(&versions)
    .fetch_all(conn)
    .await?;
    for row in rows {
        let id = row.record_id;
        out.entry(id).or_default().push(row.into_evidence()?);
    }
    Ok(out)
}

/// Attaches current evidence to record rows (keeping the row order).
async fn with_evidence(
    conn: &mut PgConnection,
    rows: Vec<RecordRow>,
) -> Result<Vec<StoredRecord>, StoreError> {
    let pairs: Vec<(KnowledgeRecordId, i32)> = rows.iter().map(|r| (r.id, r.version)).collect();
    let mut evidence = evidence_of(conn, &pairs).await?;
    rows.into_iter()
        .map(|row| {
            let e = evidence.remove(&row.id).unwrap_or_default();
            row.into_record(e)
        })
        .collect()
}

/// Looks a record up, with the evidence of its current version.
pub async fn get_record(
    conn: &mut PgConnection,
    id: KnowledgeRecordId,
) -> Result<Option<StoredRecord>, StoreError> {
    let row = sqlx::query_as::<_, RecordRow>(concat!(
        "SELECT ",
        record_columns!(),
        " FROM knowledge_record WHERE id = $1"
    ))
    .bind(id)
    .fetch_optional(&mut *conn)
    .await?;
    match row {
        None => Ok(None),
        Some(row) => Ok(with_evidence(conn, vec![row]).await?.into_iter().next()),
    }
}

/// Creates a record: its row, the content of its version, the evidence of
/// that version and its history, atomically.
///
/// Fails with [`StoreError::AlreadyExists`] for a taken id,
/// [`StoreError::NotFound`] when the organization, scope, superseding record
/// or an evidence project is missing or belongs to another organization, and
/// [`StoreError::InvalidInput`] for malformed values (the message names the
/// field, never the value).
pub async fn insert_record(
    conn: &mut PgConnection,
    record: &NewRecord,
) -> Result<StoredRecord, StoreError> {
    check_subject(&record.subject)?;
    check_content(&record.title, &record.body, &record.tags)?;
    check_labels("related symbol", &record.related_symbols)?;
    check_evidence(&record.evidence)?;
    check_history(&record.history)?;
    let version = check_version(record.version)?;
    if record.superseded_by == Some(record.id) {
        return Err(StoreError::invalid("a record cannot supersede itself"));
    }

    let mut tx = conn.begin().await?;
    let (workspace, project, task, user) =
        scope_columns(&mut tx, record.organization, &record.scope).await?;
    sqlx::query(
        "INSERT INTO knowledge_record (id, organization_id, scope_kind, workspace_id, project_id,
                                       task_id, user_key, kind, subject, title, body, state,
                                       version, author, pinned, tags, related_symbols,
                                       superseded_by, created_at, updated_at)
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14, $15, $16, $17,
                 $18, $19, $20)",
    )
    .bind(record.id)
    .bind(record.organization)
    .bind(record.scope.kind())
    .bind(workspace)
    .bind(project)
    .bind(task)
    .bind(user)
    .bind(record.kind)
    .bind(&record.subject)
    .bind(&record.title)
    .bind(&record.body)
    .bind(record.state)
    .bind(version)
    .bind(&record.author)
    .bind(record.pinned)
    .bind(&record.tags)
    .bind(&record.related_symbols)
    .bind(record.superseded_by)
    .bind(record.created_at)
    .bind(record.updated_at)
    .execute(&mut *tx)
    .await
    .map_err(|e| {
        record_write_error(
            e,
            record.id,
            record.organization,
            &record.scope,
            record.superseded_by,
        )
    })?;
    insert_version(
        &mut tx,
        record.id,
        version,
        (&record.title, &record.body, &record.tags),
        record.updated_at,
    )
    .await?;
    insert_evidence(
        &mut tx,
        record.organization,
        record.id,
        version,
        &record.evidence,
    )
    .await?;
    insert_history(&mut tx, record.id, &record.history).await?;
    let stored = get_record(&mut tx, record.id)
        .await?
        .ok_or_else(|| StoreError::Corrupt("inserted knowledge record vanished".to_owned()))?;
    tx.commit().await?;
    Ok(stored)
}

/// Applies `update` if the record still has `expected_version` and
/// `expected_revision`; otherwise fails with [`StoreError::Conflict`] and
/// changes nothing. Returns the updated record (revision + 1).
///
/// With [`RecordUpdate::content`] the content is stored as version
/// `expected_version + 1` together with its evidence; earlier versions stay
/// untouched. History entries are appended in the given order.
pub async fn update_record(
    conn: &mut PgConnection,
    update: &RecordUpdate,
) -> Result<StoredRecord, StoreError> {
    check_labels("related symbol", &update.related_symbols)?;
    check_history(&update.history)?;
    if let Some(content) = &update.content {
        check_content(&content.title, &content.body, &content.tags)?;
        check_evidence(&content.evidence)?;
    }
    if update.superseded_by == Some(update.id) {
        return Err(StoreError::invalid("a record cannot supersede itself"));
    }
    let expected_version = check_version(update.expected_version)?;
    let expected_revision = to_revision(update.expected_revision)?;
    let new_version = match update.content {
        Some(_) => expected_version
            .checked_add(1)
            .ok_or_else(|| StoreError::invalid("record version overflows"))?,
        None => expected_version,
    };

    let mut tx = conn.begin().await?;
    let current: Option<(OrganizationId, i32, i64)> = sqlx::query_as(
        "SELECT organization_id, version, revision FROM knowledge_record WHERE id = $1 FOR UPDATE",
    )
    .bind(update.id)
    .fetch_optional(&mut *tx)
    .await?;
    let Some((organization, version, revision)) = current else {
        return Err(StoreError::not_found("knowledge record", update.id));
    };
    if version != expected_version || revision != expected_revision {
        return Err(StoreError::Conflict {
            entity: "knowledge record",
            key: update.id.to_string(),
            detail: format!(
                "expected version {expected_version} revision {expected_revision}, found version {version} revision {revision}"
            ),
        });
    }
    if let Some(content) = &update.content {
        insert_version(
            &mut tx,
            update.id,
            new_version,
            (&content.title, &content.body, &content.tags),
            update.updated_at,
        )
        .await?;
        insert_evidence(
            &mut tx,
            organization,
            update.id,
            new_version,
            &content.evidence,
        )
        .await?;
    }
    let content = update.content.as_ref();
    sqlx::query(
        "UPDATE knowledge_record
         SET state = $2, pinned = $3, related_symbols = $4, superseded_by = $5, version = $6,
             revision = revision + 1, updated_at = $7,
             title = coalesce($8, title), body = coalesce($9, body), tags = coalesce($10, tags)
         WHERE id = $1",
    )
    .bind(update.id)
    .bind(update.state)
    .bind(update.pinned)
    .bind(&update.related_symbols)
    .bind(update.superseded_by)
    .bind(new_version)
    .bind(update.updated_at)
    .bind(content.map(|c| c.title.as_str()))
    .bind(content.map(|c| c.body.as_str()))
    .bind(content.map(|c| c.tags.as_slice()))
    .execute(&mut *tx)
    .await
    .map_err(|e| {
        record_write_error(
            e,
            update.id,
            organization,
            &RecordScope::Organization,
            update.superseded_by,
        )
    })?;
    insert_history(&mut tx, update.id, &update.history).await?;
    let stored = get_record(&mut tx, update.id)
        .await?
        .ok_or_else(|| StoreError::Corrupt("updated knowledge record vanished".to_owned()))?;
    tx.commit().await?;
    Ok(stored)
}

/// Deletes a record with its versions, evidence and history. Records it
/// superseded keep their state but lose the link. Returns whether it
/// existed.
pub async fn delete_record(
    conn: &mut PgConnection,
    id: KnowledgeRecordId,
) -> Result<bool, StoreError> {
    let done = sqlx::query("DELETE FROM knowledge_record WHERE id = $1")
        .bind(id)
        .execute(conn)
        .await?;
    Ok(done.rows_affected() > 0)
}

fn scope_keys(scopes: &[RecordScope]) -> Result<Vec<String>, StoreError> {
    scopes
        .iter()
        .map(|s| {
            s.validate()?;
            Ok(s.key())
        })
        .collect()
}

/// Records matching `filter`, most recently updated first (ties by id,
/// descending), each with the evidence of its current version.
pub async fn list_records(
    conn: &mut PgConnection,
    filter: &RecordFilter,
) -> Result<Vec<StoredRecord>, StoreError> {
    let limit = check_limit(filter.limit)?;
    let keys = scope_keys(&filter.scopes)?;
    let rows = sqlx::query_as::<_, RecordRow>(concat!(
        "SELECT ",
        record_columns!(),
        " FROM knowledge_record WHERE ",
        record_filter!(),
        " AND ($8::timestamptz IS NULL OR (updated_at, id) < ($8, $9::uuid))
         ORDER BY updated_at DESC, id DESC
         LIMIT $10"
    ))
    .bind(filter.organization)
    .bind(&keys)
    .bind(&filter.states)
    .bind(&filter.kinds)
    .bind(filter.subject.as_deref())
    .bind(filter.pinned)
    .bind(filter.tag.as_deref())
    .bind(filter.before.map(|c| c.updated_at))
    .bind(filter.before.map(|c| c.id))
    .bind(limit)
    .fetch_all(&mut *conn)
    .await?;
    with_evidence(conn, rows).await
}

/// Full-text search over title, subject and body (PostgreSQL `simple`
/// configuration, `websearch_to_tsquery` syntax: words, `"phrases"`,
/// `-excluded`, `or`), restricted by `filter`. Best matches first, ties by
/// most recent update, then id. `filter.before` must be `None` (ranked
/// results are not paged by cursor).
pub async fn search_records(
    conn: &mut PgConnection,
    filter: &RecordFilter,
    query: &str,
) -> Result<Vec<RecordHit>, StoreError> {
    let limit = check_limit(filter.limit)?;
    if filter.before.is_some() {
        return Err(StoreError::invalid(
            "search results are ranked and cannot be paged with a cursor",
        ));
    }
    check_text("search query", query, MAX_QUERY_BYTES, false)?;
    if query.trim().is_empty() {
        return Err(StoreError::invalid("search query must not be blank"));
    }
    let keys = scope_keys(&filter.scopes)?;
    let rows = sqlx::query_as::<_, HitRow>(concat!(
        "SELECT ",
        record_columns!(),
        ", ts_rank_cd(search, q) AS rank
         FROM knowledge_record, websearch_to_tsquery('simple', $8) AS q
         WHERE search @@ q AND ",
        record_filter!(),
        " ORDER BY rank DESC, updated_at DESC, id DESC
         LIMIT $9"
    ))
    .bind(filter.organization)
    .bind(&keys)
    .bind(&filter.states)
    .bind(&filter.kinds)
    .bind(filter.subject.as_deref())
    .bind(filter.pinned)
    .bind(filter.tag.as_deref())
    .bind(query)
    .bind(limit)
    .fetch_all(&mut *conn)
    .await?;
    let ranks: Vec<f32> = rows.iter().map(|r| r.rank).collect();
    let records = with_evidence(conn, rows.into_iter().map(|r| r.record).collect()).await?;
    Ok(records
        .into_iter()
        .zip(ranks)
        .map(|(record, rank)| RecordHit { record, rank })
        .collect())
}

/// Records of `organization` in one of `states` (empty = any) whose
/// **current** evidence cites one of the changed files at its old content
/// hash: the candidates a staleness check must look at. Ordered by id.
pub async fn records_citing_files(
    conn: &mut PgConnection,
    organization: OrganizationId,
    changes: &[EvidenceChange],
    states: &[KnowledgeState],
) -> Result<Vec<StoredRecord>, StoreError> {
    if changes.is_empty() {
        return Ok(Vec::new());
    }
    let projects: Vec<ProjectId> = changes.iter().map(|c| c.project).collect();
    let paths: Vec<&str> = changes.iter().map(|c| c.path.as_str()).collect();
    let hashes: Vec<Vec<u8>> = changes.iter().map(|c| hash_bytes(&c.old_hash)).collect();
    let rows = sqlx::query_as::<_, RecordRow>(concat!(
        "SELECT ",
        record_columns!(),
        " FROM knowledge_record
         WHERE organization_id = $1
           AND (cardinality($2::knowledge_state[]) = 0 OR state = ANY($2))
           AND id IN (
             SELECT e.record_id
             FROM unnest($3::uuid[], $4::text[], $5::bytea[]) AS c(project_id, path, content_hash)
             JOIN knowledge_evidence e
               ON e.project_id = c.project_id AND e.path = c.path
              AND e.content_hash = c.content_hash
             JOIN knowledge_record r ON r.id = e.record_id AND r.version = e.version)
         ORDER BY id"
    ))
    .bind(organization)
    .bind(states)
    .bind(&projects)
    .bind(&paths)
    .bind(&hashes)
    .fetch_all(&mut *conn)
    .await?;
    with_evidence(conn, rows).await
}

/// Records of `organization` in one of `states` (empty = any) related to at
/// least one of `symbols` (changed or removed symbol ids). Ordered by id.
pub async fn records_about_symbols(
    conn: &mut PgConnection,
    organization: OrganizationId,
    symbols: &[String],
    states: &[KnowledgeState],
) -> Result<Vec<StoredRecord>, StoreError> {
    if symbols.is_empty() {
        return Ok(Vec::new());
    }
    let rows = sqlx::query_as::<_, RecordRow>(concat!(
        "SELECT ",
        record_columns!(),
        " FROM knowledge_record
         WHERE organization_id = $1
           AND (cardinality($2::knowledge_state[]) = 0 OR state = ANY($2))
           AND related_symbols && $3::text[]
         ORDER BY id"
    ))
    .bind(organization)
    .bind(states)
    .bind(symbols)
    .fetch_all(&mut *conn)
    .await?;
    with_evidence(conn, rows).await
}

/// Every stored content version of a record, oldest first, each with its
/// evidence. Empty when the record does not exist.
pub async fn record_versions(
    conn: &mut PgConnection,
    id: KnowledgeRecordId,
) -> Result<Vec<RecordVersion>, StoreError> {
    let rows = sqlx::query_as::<_, VersionRow>(
        "SELECT version, title, body, tags, created_at FROM knowledge_record_version
         WHERE record_id = $1 ORDER BY version",
    )
    .bind(id)
    .fetch_all(&mut *conn)
    .await?;
    let evidence_rows = sqlx::query_as::<_, EvidenceRow>(
        "SELECT record_id, version, project_id, view_key, commit_id, path, start_line, end_line,
                content_hash
         FROM knowledge_evidence WHERE record_id = $1 ORDER BY version, ordinal",
    )
    .bind(id)
    .fetch_all(&mut *conn)
    .await?;
    let mut evidence: BTreeMap<i32, Vec<RecordEvidence>> = BTreeMap::new();
    for row in evidence_rows {
        let version = row.version;
        evidence
            .entry(version)
            .or_default()
            .push(row.into_evidence()?);
    }
    let replaced: Vec<Option<OffsetDateTime>> = rows
        .iter()
        .skip(1)
        .map(|r| Some(r.created_at))
        .chain(std::iter::once(None))
        .collect();
    rows.into_iter()
        .zip(replaced)
        .map(|(row, replaced_at)| {
            Ok(RecordVersion {
                version: from_i32(row.version, "record version")?,
                evidence: evidence.remove(&row.version).unwrap_or_default(),
                title: row.title,
                body: row.body,
                tags: row.tags,
                created_at: row.created_at,
                replaced_at,
            })
        })
        .collect()
}

/// The history of a record, oldest first. Empty when the record does not
/// exist.
pub async fn record_history(
    conn: &mut PgConnection,
    id: KnowledgeRecordId,
) -> Result<Vec<HistoryEntry>, StoreError> {
    let rows = sqlx::query_as::<_, HistoryRow>(
        "SELECT at, actor, action, from_state, to_state, version, reason
         FROM knowledge_history WHERE record_id = $1 ORDER BY id",
    )
    .bind(id)
    .fetch_all(conn)
    .await?;
    rows.into_iter()
        .map(|row| {
            Ok(HistoryEntry {
                at: row.at,
                actor: row.actor,
                action: row.action,
                from: row.from_state,
                to: row.to_state,
                version: from_i32(row.version, "history version")?,
                reason: row.reason,
            })
        })
        .collect()
}

/// A fresh record id (UUIDv7, time ordered), for callers that do not take
/// ids from the knowledge domain.
pub fn new_record_id() -> KnowledgeRecordId {
    KnowledgeRecordId(Uuid::now_v7())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn subjects_are_canonical() {
        for ok in [
            "payments",
            "payments.idempotency",
            "a-b_c.d1",
            &"x".repeat(128),
        ] {
            assert!(check_subject(ok).is_ok(), "{ok}");
        }
        for bad in [
            "",
            ".a",
            "a.",
            "a..b",
            "Payments",
            "a b",
            "ödeme",
            &"x".repeat(129),
        ] {
            assert!(check_subject(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn commits_and_labels_are_checked() {
        assert!(check_commit("abc1234").is_ok());
        assert!(check_commit(&"f".repeat(64)).is_ok());
        for bad in ["abc123", "ABC1234", "xyz1234", &"a".repeat(65)] {
            assert!(check_commit(bad).is_err(), "{bad}");
        }
        assert!(check_label("tag", "rule", MAX_LABEL_BYTES).is_ok());
        for bad in ["", " x", "x ", "a\nb", "a\0b"] {
            assert!(check_label("tag", bad, MAX_LABEL_BYTES).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn scope_keys_are_canonical() {
        let id = Uuid::from_u128(1);
        assert_eq!(RecordScope::Organization.key(), "org");
        assert_eq!(
            RecordScope::Workspace(WorkspaceId(id)).key(),
            "workspace:00000000-0000-0000-0000-000000000001"
        );
        assert_eq!(
            RecordScope::Project(ProjectId(id)).key(),
            "project:00000000-0000-0000-0000-000000000001"
        );
        assert_eq!(RecordScope::User("u-1".into()).key(), "user:u-1");
        assert_eq!(
            RecordScope::Task(TaskId(id)).kind(),
            KnowledgeScopeKind::Task
        );
        assert!(RecordScope::User(String::new()).validate().is_err());
    }

    #[test]
    fn limits_are_enforced() {
        assert!(check_limit(0).is_err());
        assert!(check_limit(MAX_RECORDS_LISTED + 1).is_err());
        assert_eq!(check_limit(5).unwrap(), 5);
        assert!(check_version(0).is_err());
        assert!(check_version(u32::MAX).is_err());
        assert!(check_content("", "", &[]).is_err());
        assert!(check_content("t", &"b".repeat(MAX_BODY_BYTES + 1), &[]).is_err());
        assert!(check_content("t", "", &["ok".into()]).is_ok());
    }
}
