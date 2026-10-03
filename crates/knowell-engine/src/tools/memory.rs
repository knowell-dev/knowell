//! `read_memory`, `write_memory`, `resume_task`, `save_checkpoint` and the
//! conversions between `knowell-knowledge` records and MCP records.
//!
//! Permission enforcement point 3: every memory read is limited to scopes
//! the caller can see (organization, the workspace, its visible projects,
//! its own tasks and its own user scope); writes are authorised with
//! `ProposeMemory` / `WriteTask` on the concrete scope. MCP callers are
//! agents, so the acceptance policy keeps every agent write `proposed`.

use std::collections::{BTreeMap, BTreeSet};

use knowell_auth::{Action, Resource};
use knowell_core::{ContentHash, Name, TrackTarget};
use knowell_knowledge::{
    Actor, Checkpoint as DomainCheckpoint, CommitId as KCommitId, DecisionProblem,
    Evidence as KEvidence, KnowledgeRecord, ManifestChangeKind, ManifestPin, NewRecord, RecordId,
    RecordKind, RecordState, Rights, Scope, Subject, SymbolId, Task, TaskId as KTaskId,
    TaskStatus as KTaskStatus, Timestamp as KTimestamp, ViewId as KViewId, detect_conflicts,
};
use knowell_mcp::tools::{
    Author, AuthorKind, ChangeKind, Checkpoint, MemoryHit, MemoryKind, MemoryRecord, MemoryScope,
    MemoryStatus, ReadMemoryInput, ReadMemoryOutput, ResumeTaskInput, ResumeTaskOutput,
    SaveCheckpointInput, SaveCheckpointOutput, ScopeLevel, SourceChange, TaskDetail, TaskStatus,
    TaskSummary, WriteMemoryInput, WriteMemoryOutput,
};
use knowell_mcp::{
    CheckpointId, CommitId, Evidence, FreshnessTier, Gap, GapReason, IndexState, MatchReason,
    MemoryId, ResultId, TaskId, Timestamp, ToolError, UntrustedText, ViewLayer,
};
use knowell_source::git::{Change, GitRepo};
use knowell_store::content;
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;
use uuid::Uuid;

use crate::access::Access;
use crate::engine::Engine;
use crate::error::store_tool;
use crate::ids::parse_source_id;
use crate::memory::{MemoryError, RecordQuery, RecordRow, TaskRow};
use crate::scope::Pinned;

/// The current time as a knowledge timestamp.
pub(crate) fn now_timestamp() -> KTimestamp {
    KTimestamp::from_datetime(OffsetDateTime::now_utc())
}

/// A knowledge timestamp as an MCP RFC 3339 timestamp.
pub(crate) fn mcp_time(at: KTimestamp) -> Result<Timestamp, ToolError> {
    let text = at
        .to_datetime()
        .and_then(|t| t.format(&Rfc3339).ok())
        .ok_or_else(|| ToolError::internal("timestamp out of range"))?;
    Timestamp::new(text).map_err(|e| ToolError::internal(format!("timestamp: {e}")))
}

pub(crate) fn memory_error(error: MemoryError) -> ToolError {
    match error {
        MemoryError::NotFound(what) => ToolError::not_found(what),
        MemoryError::Conflict(what) => {
            ToolError::invalid_input(format!("{what}; read it again and retry"))
        }
        other => ToolError::internal(other.to_string()),
    }
}

/// Maps a knowledge error (validation, secret guard, policy) to a tool
/// error. Secret findings name the field, kind and line, never the value.
pub(crate) fn knowledge_error(error: knowell_knowledge::KnowledgeError) -> ToolError {
    match error {
        knowell_knowledge::KnowledgeError::NotAuthorized { .. } => {
            ToolError::permission_denied(error.to_string())
        }
        other => ToolError::invalid_input(other.to_string()),
    }
}

/// The MCP kind of a record: its kind tag, else what its origin implies.
fn kind_of(record: &KnowledgeRecord) -> MemoryKind {
    for tag in &record.tags {
        let kind = match tag.as_str() {
            "decision" => MemoryKind::Decision,
            "rule" => MemoryKind::Rule,
            "example" => MemoryKind::Example,
            "finding" => MemoryKind::Finding,
            "note" => MemoryKind::Note,
            "observation" => MemoryKind::Observation,
            "description" => MemoryKind::Description,
            _ => continue,
        };
        return kind;
    }
    match record.kind {
        RecordKind::Observed => MemoryKind::Observation,
        RecordKind::Human => MemoryKind::Decision,
        RecordKind::ModelSuggestion => MemoryKind::Finding,
    }
}

fn kind_tag(kind: MemoryKind) -> &'static str {
    match kind {
        MemoryKind::Decision => "decision",
        MemoryKind::Rule => "rule",
        MemoryKind::Example => "example",
        MemoryKind::Finding => "finding",
        MemoryKind::Note => "note",
        MemoryKind::Observation => "observation",
        MemoryKind::Description => "description",
    }
}

pub(crate) fn status_of(state: RecordState) -> MemoryStatus {
    match state {
        RecordState::Proposed => MemoryStatus::Proposed,
        RecordState::Accepted => MemoryStatus::Accepted,
        RecordState::Rejected => MemoryStatus::Rejected,
        RecordState::Stale => MemoryStatus::Stale,
        RecordState::Superseded => MemoryStatus::Superseded,
    }
}

fn state_of(status: MemoryStatus) -> RecordState {
    match status {
        MemoryStatus::Proposed => RecordState::Proposed,
        MemoryStatus::Accepted => RecordState::Accepted,
        MemoryStatus::Rejected => RecordState::Rejected,
        MemoryStatus::Stale => RecordState::Stale,
        MemoryStatus::Superseded => RecordState::Superseded,
    }
}

fn author_of(actor: &Actor) -> Author {
    match actor {
        Actor::Human(user) => Author {
            kind: AuthorKind::Human,
            name: format!("user:{user}"),
            session: None,
        },
        Actor::Agent { session, client } => Author {
            kind: AuthorKind::Agent,
            name: client.to_string(),
            session: Some(session.to_string()),
        },
        Actor::System => Author {
            kind: AuthorKind::System,
            name: "knowell".to_owned(),
            session: None,
        },
    }
}

fn mcp_scope(scope: &Scope) -> MemoryScope {
    match scope {
        Scope::Organization => MemoryScope {
            level: ScopeLevel::Organization,
            project: None,
            task_id: None,
        },
        Scope::Workspace(_) => MemoryScope {
            level: ScopeLevel::Workspace,
            project: None,
            task_id: None,
        },
        Scope::Project { project, .. } => MemoryScope {
            level: ScopeLevel::Project,
            project: Some(project.clone()),
            task_id: None,
        },
        Scope::Task(task) => MemoryScope {
            level: ScopeLevel::Task,
            project: None,
            task_id: TaskId::new(task.to_string()).ok(),
        },
        Scope::User(_) => MemoryScope {
            level: ScopeLevel::User,
            project: None,
            task_id: None,
        },
    }
}

/// MCP evidence of a record's code pointer; `None` when the stored commit
/// is not a full id or the ref text does not parse.
fn record_evidence(e: &KEvidence, pinned: Option<&Pinned>) -> Option<Evidence> {
    let commit = CommitId::new(e.commit.as_str()).ok()?;
    let view: TrackTarget = e.view.as_str().parse().ok()?;
    let index_state = match pinned.and_then(|p| p.projects.get(&e.project)) {
        Some(project) if project.commit.as_deref() == Some(e.commit.as_str()) => {
            IndexState::Current
        }
        Some(_) => IndexState::Stale,
        None => IndexState::NotIndexed,
    };
    Some(Evidence {
        project: e.project.clone(),
        view,
        layer: ViewLayer::Shared,
        commit,
        path: e.path.clone(),
        lines: e.range,
        content_hash: e.content_hash,
        symbol: None,
        why: Vec::new(),
        freshness: FreshnessTier::T1Symbols,
        index_state,
    })
}

/// A knowledge record as an MCP memory record (body labelled untrusted).
pub(crate) fn memory_record(
    record: &KnowledgeRecord,
    conflicts: &[RecordId],
    pinned: Option<&Pinned>,
) -> Result<MemoryRecord, ToolError> {
    let mut related_projects: BTreeSet<Name> =
        record.evidence.iter().map(|e| e.project.clone()).collect();
    if let Scope::Project { project, .. } = &record.scope {
        related_projects.insert(project.clone());
    }
    Ok(MemoryRecord {
        id: MemoryId::new(record.id.to_string()).map_err(|e| ToolError::internal(e.to_string()))?,
        version: record.version,
        scope: mcp_scope(&record.scope),
        kind: kind_of(record),
        status: status_of(record.state),
        title: record.title.clone(),
        body: UntrustedText::memory(record.body.clone()),
        author: author_of(&record.author),
        created_at: mcp_time(record.created_at)?,
        updated_at: mcp_time(record.updated_at)?,
        related_projects: related_projects.into_iter().collect(),
        related_symbols: record
            .related_symbols
            .iter()
            .map(|s| s.as_str().to_owned())
            .collect(),
        evidence: record
            .evidence
            .iter()
            .filter_map(|e| record_evidence(e, pinned))
            .collect(),
        superseded_by: record
            .superseded_by
            .and_then(|id| MemoryId::new(id.to_string()).ok()),
        conflicts_with: conflicts
            .iter()
            .filter_map(|id| MemoryId::new(id.to_string()).ok())
            .collect(),
    })
}

/// Conflicting record ids per record, among `records`.
pub(crate) fn conflict_map(records: &[KnowledgeRecord]) -> BTreeMap<RecordId, Vec<RecordId>> {
    let mut out: BTreeMap<RecordId, Vec<RecordId>> = BTreeMap::new();
    for conflict in detect_conflicts(records) {
        out.entry(conflict.first).or_default().push(conflict.second);
        out.entry(conflict.second).or_default().push(conflict.first);
    }
    out
}

/// A record subject derived from a title: up to six ASCII words joined by
/// `.`; the kind tag when nothing usable remains.
fn subject_from(title: &str, kind: MemoryKind) -> Result<Subject, ToolError> {
    let words: Vec<String> = title
        .split(|c: char| !c.is_ascii_alphanumeric())
        .filter(|w| !w.is_empty())
        .take(6)
        .map(str::to_ascii_lowercase)
        .collect();
    let mut text = words.join(".");
    text.truncate(Subject::MAX_LEN);
    let text = text.trim_end_matches('.').to_owned();
    Subject::new(if text.is_empty() {
        kind_tag(kind)
    } else {
        &text
    })
    .map_err(|e| ToolError::internal(format!("subject: {e}")))
}

/// A UUID derived from an identity and a client key, for idempotent writes.
fn idempotent_uuid(namespace: &str, owner: &str, key: &str) -> Uuid {
    let digest = ContentHash::of_parts([
        namespace.as_bytes(),
        owner.as_bytes(),
        b"\0".as_slice(),
        key.as_bytes(),
    ]);
    let mut bytes = [0u8; 16];
    for (out, b) in bytes.iter_mut().zip(digest.as_bytes().iter()) {
        *out = *b;
    }
    uuid::Builder::from_random_bytes(bytes).into_uuid()
}

fn parse_task_id(id: &TaskId) -> Result<KTaskId, ToolError> {
    Uuid::parse_str(id.as_str())
        .map(KTaskId::from_uuid)
        .map_err(|_| ToolError::not_found(format!("task {id} does not exist")))
}

fn parse_record_id(id: &MemoryId) -> Option<RecordId> {
    Uuid::parse_str(id.as_str()).ok().map(RecordId::from_uuid)
}

fn mcp_task_status(status: KTaskStatus) -> TaskStatus {
    match status {
        KTaskStatus::Open | KTaskStatus::InProgress => TaskStatus::InProgress,
        KTaskStatus::Blocked => TaskStatus::Blocked,
        KTaskStatus::Done => TaskStatus::Done,
        KTaskStatus::Abandoned => TaskStatus::Abandoned,
    }
}

fn domain_task_statuses(statuses: &[TaskStatus]) -> Vec<KTaskStatus> {
    let mut out = Vec::new();
    for status in statuses {
        match status {
            TaskStatus::InProgress => {
                out.push(KTaskStatus::Open);
                out.push(KTaskStatus::InProgress);
            }
            TaskStatus::Blocked => out.push(KTaskStatus::Blocked),
            TaskStatus::Done => out.push(KTaskStatus::Done),
            TaskStatus::Abandoned => out.push(KTaskStatus::Abandoned),
        }
    }
    out
}

fn owner_author(task: &Task) -> Author {
    task.notes
        .first()
        .map_or_else(|| author_of(&Actor::System), |n| author_of(&n.author))
}

/// A task as an MCP summary.
pub(crate) fn task_summary(row: &TaskRow, last_seq: Option<u64>) -> Result<TaskSummary, ToolError> {
    let task_id =
        TaskId::new(row.task.id.to_string()).map_err(|e| ToolError::internal(e.to_string()))?;
    Ok(TaskSummary {
        last_checkpoint: last_seq.and_then(|seq| checkpoint_id(&row.task.id, seq).ok()),
        task_id,
        title: row.task.title.clone(),
        goal: UntrustedText::memory(row.task.goal.clone()),
        status: mcp_task_status(row.task.status),
        owner: owner_author(&row.task),
        updated_at: mcp_time(row.task.updated_at)?,
    })
}

fn checkpoint_id(task: &KTaskId, seq: u64) -> Result<CheckpointId, ToolError> {
    CheckpointId::new(format!("cp-{}-{seq}", task.as_uuid().simple()))
        .map_err(|e| ToolError::internal(e.to_string()))
}

/// Whether the task belongs to `access`'s user (tasks without an owner are
/// shared).
fn owns(row: &TaskRow, access: &Access) -> bool {
    match (&row.owner, access.acting_user()) {
        (None, _) => true,
        (Some(owner), Some(user)) => *owner == user.to_string(),
        (Some(_), None) => false,
    }
}

impl Engine {
    /// The memory scopes `access` may read in `pinned`'s workspace: the
    /// organization, the workspace, every visible project and its own user
    /// scope (task scopes are added per task).
    pub(crate) fn readable_scopes(&self, access: &Access, pinned: &Pinned) -> Vec<Scope> {
        let ws = &pinned.workspace.name;
        let mut scopes = Vec::new();
        if access.allows(Action::ReadMemory, &Resource::workspace(ws.clone())) {
            scopes.push(Scope::Organization);
            scopes.push(Scope::Workspace(ws.clone()));
        }
        for project in &pinned.workspace.projects {
            if access.reads_project(ws, &project.name)
                && access.allows(
                    Action::ReadMemory,
                    &Resource::project(ws.clone(), project.name.clone()),
                )
            {
                scopes.push(Scope::Project {
                    workspace: ws.clone(),
                    project: project.name.clone(),
                });
            }
        }
        if let Some(user) = access.acting_user()
            && let Ok(user) = knowell_knowledge::UserId::new(user.to_string())
        {
            scopes.push(Scope::User(user));
        }
        scopes
    }

    /// Tasks of the workspace that `access` owns (or that have no owner).
    async fn own_tasks(
        &self,
        access: &Access,
        pinned: &Pinned,
        statuses: &[KTaskStatus],
        limit: u32,
    ) -> Result<Vec<TaskRow>, ToolError> {
        let rows = self
            .inner
            .memory
            .list_tasks(
                Some(&pinned.workspace.name),
                statuses,
                limit.saturating_mul(4),
            )
            .await
            .map_err(memory_error)?;
        Ok(rows.into_iter().filter(|r| owns(r, access)).collect())
    }

    /// A task the caller may use, or not found.
    async fn own_task(
        &self,
        access: &Access,
        pinned: &Pinned,
        id: KTaskId,
    ) -> Result<TaskRow, ToolError> {
        let row = self
            .inner
            .memory
            .get_task(id)
            .await
            .map_err(memory_error)?
            .filter(|r| r.workspace.as_ref() == Some(&pinned.workspace.name) && owns(r, access))
            .ok_or_else(|| ToolError::not_found(format!("task {id} does not exist")))?;
        Ok(row)
    }

    /// Records in `scopes`, filtered.
    async fn records_in(
        &self,
        scopes: Vec<Scope>,
        states: Vec<RecordState>,
        text: Option<String>,
        limit: u32,
    ) -> Result<Vec<RecordRow>, ToolError> {
        if scopes.is_empty() {
            return Ok(Vec::new());
        }
        let query = RecordQuery {
            scopes,
            states,
            kinds: Vec::new(),
            text,
            limit,
        };
        self.inner
            .memory
            .find_records(&query)
            .await
            .map_err(memory_error)
    }

    /// Accepted rules and decisions plus open tasks for session bootstrap.
    pub(crate) async fn bootstrap_inputs(
        &self,
        access: &Access,
        pinned: &Pinned,
    ) -> Result<(Vec<KnowledgeRecord>, Vec<TaskRow>), ToolError> {
        let scopes = self.readable_scopes(access, pinned);
        let records = self
            .records_in(
                scopes,
                vec![RecordState::Accepted, RecordState::Stale],
                None,
                500,
            )
            .await?
            .into_iter()
            .map(|r| r.record)
            .collect();
        let tasks = self
            .own_tasks(
                access,
                pinned,
                &[
                    KTaskStatus::Open,
                    KTaskStatus::InProgress,
                    KTaskStatus::Blocked,
                ],
                50,
            )
            .await?;
        Ok((records, tasks))
    }

    /// Resolves memory evidence ids against the pinned views.
    async fn evidence_from_ids(
        &self,
        pinned: &Pinned,
        ids: &[ResultId],
    ) -> Result<Vec<KEvidence>, ToolError> {
        let mut out = Vec::new();
        for id in ids {
            let unresolved = || {
                ToolError::invalid_input(format!(
                    "evidence id {id} does not resolve in the context's views"
                ))
            };
            let source = parse_source_id(id).ok_or_else(unresolved)?;
            let project = pinned
                .projects
                .get(&source.project)
                .ok_or_else(unresolved)?;
            let snapshot = self.snapshot_of(project).await?;
            let path = snapshot
                .files
                .keys()
                .find(|p| source.path.matches(p))
                .cloned()
                .ok_or_else(unresolved)?;
            let current = snapshot.file(&path).ok_or_else(unresolved)?;
            let hash = if source.names_version(&current.content_hash) {
                current.content_hash
            } else {
                let mut conn = self.inner.store.acquire().await.map_err(store_tool)?;
                content::file_history(&mut conn, project.view, &path, 200)
                    .await
                    .map_err(store_tool)?
                    .into_iter()
                    .map(|v| v.content_hash)
                    .find(|h| source.names_version(h))
                    .ok_or_else(unresolved)?
            };
            let commit = project.commit.clone().ok_or_else(|| {
                ToolError::invalid_input(format!("{} has no commit to cite", source.project))
            })?;
            out.push(KEvidence {
                project: source.project.clone(),
                view: KViewId::new(project.target.to_string())
                    .map_err(|e| ToolError::internal(e.to_string()))?,
                commit: KCommitId::new(commit).map_err(|e| ToolError::internal(e.to_string()))?,
                path,
                range: source.lines,
                content_hash: hash,
            });
        }
        Ok(out)
    }

    pub(crate) async fn tool_read_memory(
        &self,
        access: Access,
        input: ReadMemoryInput,
    ) -> Result<ReadMemoryOutput, ToolError> {
        let pinned = self.resolve_target(&access, &input.target).await?;
        let limit = input.limit.unwrap_or(20);
        let mut scopes = self.readable_scopes(&access, &pinned);
        if let Some(project) = &input.project {
            if !access.reads_project(&pinned.workspace.name, project) {
                return Err(ToolError::not_found(format!(
                    "project {project} does not exist"
                )));
            }
            scopes.retain(|s| match s {
                Scope::Project { project: p, .. } => p == project,
                _ => true,
            });
        }
        if let Some(task) = &input.task_id {
            let row = self
                .own_task(&access, &pinned, parse_task_id(task)?)
                .await?;
            scopes.push(Scope::Task(row.task.id));
        }
        if !input.scopes.is_empty() {
            scopes.retain(|s| {
                let level = mcp_scope(s).level;
                input.scopes.contains(&level)
            });
        }
        let states: Vec<RecordState> = if input.statuses.is_empty() {
            vec![RecordState::Accepted, RecordState::Proposed]
        } else {
            input.statuses.iter().copied().map(state_of).collect()
        };
        let mut rows: Vec<RecordRow> = Vec::new();
        if input.ids.is_empty() {
            rows = self
                .records_in(
                    scopes.clone(),
                    states.clone(),
                    input.query.clone(),
                    limit.saturating_add(1),
                )
                .await?;
        } else {
            for id in &input.ids {
                let Some(record_id) = parse_record_id(id) else {
                    continue;
                };
                if let Some(row) = self
                    .inner
                    .memory
                    .get_record(record_id)
                    .await
                    .map_err(memory_error)?
                    && scopes.contains(&row.record.scope)
                    && states.contains(&row.record.state)
                {
                    rows.push(row);
                }
            }
        }
        rows.retain(|r| input.kinds.is_empty() || input.kinds.contains(&kind_of(&r.record)));
        let more_available = rows.len() > usize::try_from(limit).unwrap_or(usize::MAX);
        rows.truncate(usize::try_from(limit).unwrap_or(usize::MAX));
        let records: Vec<KnowledgeRecord> = rows.into_iter().map(|r| r.record).collect();
        let conflicts = conflict_map(&records);
        let mut out = Vec::new();
        for record in &records {
            let empty = Vec::new();
            out.push(memory_record(
                record,
                conflicts.get(&record.id).unwrap_or(&empty),
                Some(&pinned),
            )?);
        }
        let mut gaps = Vec::new();
        if out.is_empty() {
            gaps.push(Gap::new(
                GapReason::NoMatches,
                "no memory record in the readable scopes matches",
            ));
        }
        if more_available {
            gaps.push(Gap::new(
                GapReason::LimitReached,
                "more records exist; raise `limit`",
            ));
        }
        Ok(ReadMemoryOutput {
            records: out,
            more_available,
            gaps,
        })
    }

    /// Memory hits for `search` (accepted and proposed records whose text
    /// matches the query words).
    pub(crate) async fn memory_hits(
        &self,
        access: &Access,
        pinned: &Pinned,
        projects: &[Name],
        query: &str,
        limit: usize,
    ) -> Result<Vec<MemoryHit>, ToolError> {
        let words: Vec<String> = query
            .split(|c: char| !c.is_alphanumeric())
            .filter(|w| w.chars().count() >= 3)
            .map(str::to_lowercase)
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect();
        if words.is_empty() {
            return Ok(Vec::new());
        }
        // Narrow before retrieval and its per-word limit, so unrelated
        // project records cannot crowd out the selected project's memory.
        // Wider scopes and unindexed selected projects remain reachable.
        let scopes: Vec<Scope> = self
            .readable_scopes(access, pinned)
            .into_iter()
            .filter(|scope| match scope {
                Scope::Project { project, .. } => projects.is_empty() || projects.contains(project),
                _ => true,
            })
            .collect();
        let mut by_id: BTreeMap<RecordId, (usize, KnowledgeRecord)> = BTreeMap::new();
        for word in &words {
            for row in self
                .records_in(
                    scopes.clone(),
                    vec![RecordState::Accepted, RecordState::Proposed],
                    Some(word.clone()),
                    50,
                )
                .await?
            {
                let entry = by_id.entry(row.record.id).or_insert((0, row.record));
                entry.0 = entry.0.saturating_add(1);
            }
        }
        let mut ranked: Vec<(usize, KnowledgeRecord)> = by_id.into_values().collect();
        ranked.sort_by(|(a, ra), (b, rb)| {
            b.cmp(a)
                .then_with(|| rb.updated_at.cmp(&ra.updated_at))
                .then_with(|| ra.id.cmp(&rb.id))
        });
        ranked.truncate(limit);
        let mut hits = Vec::new();
        for (rank, (_, record)) in ranked.iter().enumerate() {
            let haystack = format!("{} {}", record.title, record.body).to_lowercase();
            let terms: Vec<String> = words
                .iter()
                .filter(|w| haystack.contains(w.as_str()))
                .cloned()
                .collect();
            hits.push(MemoryHit {
                record: memory_record(record, &[], Some(pinned))?,
                why: vec![MatchReason::Lexical {
                    terms,
                    rank: u32::try_from(rank.saturating_add(1)).unwrap_or(u32::MAX),
                }],
            });
        }
        Ok(hits)
    }

    pub(crate) async fn tool_write_memory(
        &self,
        access: Access,
        input: WriteMemoryInput,
    ) -> Result<WriteMemoryOutput, ToolError> {
        let pinned = self.resolve_target(&access, &input.target).await?;
        let ws = pinned.workspace.name.clone();
        let (scope, resource) = match input.scope.level {
            ScopeLevel::Organization => (Scope::Organization, Resource::Organization),
            ScopeLevel::Workspace => (
                Scope::Workspace(ws.clone()),
                Resource::workspace(ws.clone()),
            ),
            ScopeLevel::Project => {
                let project = input.scope.project.clone().ok_or_else(|| {
                    ToolError::invalid_input("project scope needs `scope.project`")
                })?;
                if !access.reads_project(&ws, &project) {
                    return Err(ToolError::not_found(format!(
                        "project {project} does not exist"
                    )));
                }
                (
                    Scope::Project {
                        workspace: ws.clone(),
                        project: project.clone(),
                    },
                    Resource::project(ws.clone(), project),
                )
            }
            ScopeLevel::Task => {
                let task =
                    input.scope.task_id.as_ref().ok_or_else(|| {
                        ToolError::invalid_input("task scope needs `scope.task_id`")
                    })?;
                let row = self
                    .own_task(&access, &pinned, parse_task_id(task)?)
                    .await?;
                (Scope::Task(row.task.id), Resource::workspace(ws.clone()))
            }
            ScopeLevel::User => {
                let user = access
                    .acting_user()
                    .ok_or_else(|| ToolError::permission_denied("only users have a user scope"))?;
                let user = knowell_knowledge::UserId::new(user.to_string())
                    .map_err(|e| ToolError::internal(e.to_string()))?;
                (Scope::User(user), Resource::workspace(ws.clone()))
            }
        };
        if !access.allows(Action::ProposeMemory, &resource) {
            return Err(ToolError::permission_denied(format!(
                "you may not write memory in this {} scope",
                match input.scope.level {
                    ScopeLevel::Organization => "organization",
                    ScopeLevel::Workspace => "workspace",
                    ScopeLevel::Project => "project",
                    ScopeLevel::Task => "task",
                    ScopeLevel::User => "user",
                }
            )));
        }
        let record_id = match &input.idempotency_key {
            Some(key) => RecordId::from_uuid(idempotent_uuid(
                "knowell.engine.memory.v1",
                access.label(),
                key,
            )),
            None => RecordId::generate(),
        };
        if input.idempotency_key.is_some()
            && let Some(existing) = self
                .inner
                .memory
                .get_record(record_id)
                .await
                .map_err(memory_error)?
        {
            return Ok(WriteMemoryOutput {
                record: memory_record(&existing.record, &[], Some(&pinned))?,
                created: false,
            });
        }
        let actor = access.actor()?;
        let evidence = self.evidence_from_ids(&pinned, &input.evidence).await?;
        let mut tags = vec![kind_tag(input.kind).to_owned()];
        if let Some(old) = &input.supersedes {
            let old_id = parse_record_id(old).ok_or_else(|| {
                ToolError::not_found(format!("memory record {old} does not exist"))
            })?;
            let readable = self.readable_scopes(&access, &pinned);
            let visible = self
                .inner
                .memory
                .get_record(old_id)
                .await
                .map_err(memory_error)?
                .is_some_and(|r| readable.contains(&r.record.scope));
            if !visible {
                return Err(ToolError::not_found(format!(
                    "memory record {old} does not exist"
                )));
            }
            tags.push(format!("supersedes:{old_id}"));
        }
        let related_symbols = input
            .related_symbols
            .iter()
            .map(|s| SymbolId::new(s.trim()).map_err(|e| ToolError::invalid_input(e.to_string())))
            .collect::<Result<Vec<_>, _>>()?;
        let kind = if access.is_agent() {
            RecordKind::ModelSuggestion
        } else {
            RecordKind::Human
        };
        let rights = Rights {
            can_accept: access.allows(Action::AcceptMemory, &resource),
        };
        let outcome = KnowledgeRecord::write(
            NewRecord {
                id: record_id,
                scope,
                kind,
                subject: subject_from(&input.title, input.kind)?,
                title: input.title.clone(),
                body: input.body.clone(),
                evidence,
                related_symbols,
                tags,
                pinned: false,
            },
            actor,
            rights,
            &self.inner.settings.acceptance,
            now_timestamp(),
        )
        .map_err(knowledge_error)?;
        let stored = match self.inner.memory.insert_record(&outcome.record).await {
            Ok(row) => row,
            Err(MemoryError::AlreadyExists(_)) => self
                .inner
                .memory
                .get_record(record_id)
                .await
                .map_err(memory_error)?
                .ok_or_else(|| ToolError::internal("record vanished after a duplicate insert"))?,
            Err(other) => return Err(memory_error(other)),
        };
        Ok(WriteMemoryOutput {
            record: memory_record(&stored.record, &[], Some(&pinned))?,
            created: true,
        })
    }

    /// The current manifest as knowledge pins.
    fn manifest_pins(pinned: &Pinned) -> Vec<ManifestPin> {
        pinned
            .projects
            .values()
            .filter_map(|p| {
                Some(ManifestPin {
                    project: p.entry.name.clone(),
                    view: KViewId::new(p.target.to_string()).ok()?,
                    commit: KCommitId::new(p.commit.clone()?).ok()?,
                    local_generation: p.overlay.as_ref().map(|o| o.generation),
                })
            })
            .collect()
    }

    pub(crate) async fn tool_resume_task(
        &self,
        access: Access,
        input: ResumeTaskInput,
    ) -> Result<ResumeTaskOutput, ToolError> {
        let pinned = self.resolve_target(&access, &input.target).await?;
        let limit = input.limit.unwrap_or(10);
        let Some(task_id) = &input.task_id else {
            let statuses = if input.statuses.is_empty() {
                vec![
                    KTaskStatus::Open,
                    KTaskStatus::InProgress,
                    KTaskStatus::Blocked,
                ]
            } else {
                domain_task_statuses(&input.statuses)
            };
            let mut rows = self.own_tasks(&access, &pinned, &statuses, limit).await?;
            if let Some(query) = &input.query {
                let words: Vec<String> = query.split_whitespace().map(str::to_lowercase).collect();
                rows.retain(|r| {
                    let text = format!("{} {}", r.task.title, r.task.goal).to_lowercase();
                    words.iter().all(|w| text.contains(w.as_str()))
                });
            }
            rows.truncate(usize::try_from(limit).unwrap_or(usize::MAX));
            let mut tasks = Vec::new();
            for row in &rows {
                let last = self
                    .inner
                    .memory
                    .checkpoints(row.task.id)
                    .await
                    .map_err(memory_error)?
                    .last()
                    .map(|c| c.seq);
                tasks.push(task_summary(row, last)?);
            }
            let gaps = if tasks.is_empty() {
                vec![Gap::new(
                    GapReason::NoMatches,
                    "no task matches in this workspace",
                )]
            } else {
                Vec::new()
            };
            return Ok(ResumeTaskOutput {
                tasks,
                task: None,
                gaps,
            });
        };
        let row = self
            .own_task(&access, &pinned, parse_task_id(task_id)?)
            .await?;
        let checkpoint_rows = self
            .inner
            .memory
            .checkpoints(row.task.id)
            .await
            .map_err(memory_error)?;
        let checkpoints: Vec<DomainCheckpoint> = checkpoint_rows
            .iter()
            .map(|c| c.checkpoint.clone())
            .collect();
        let current = Self::manifest_pins(&pinned);
        let mut records = Vec::new();
        for id in &row.task.decisions {
            if let Some(r) = self
                .inner
                .memory
                .get_record(*id)
                .await
                .map_err(memory_error)?
            {
                records.push(r.record);
            }
        }
        let stale = self
            .records_in(
                self.readable_scopes(&access, &pinned),
                vec![RecordState::Stale],
                None,
                200,
            )
            .await?;
        records.extend(stale.into_iter().map(|r| r.record));
        let digest = knowell_knowledge::resume(&row.task, &checkpoints, &current, &records);
        let mut gaps = Vec::new();
        let mut changed_since = Vec::new();
        for change in &digest.manifest_changes {
            let ManifestChangeKind::Moved {
                from_commit,
                to_commit,
                ..
            } = &change.kind
            else {
                continue;
            };
            match self
                .changes_between(
                    &pinned,
                    &change.project,
                    from_commit.as_str(),
                    to_commit.as_str(),
                )
                .await
            {
                Ok(list) => changed_since.extend(list),
                Err(message) => gaps.push(Gap::for_project(
                    GapReason::NotFound,
                    change.project.clone(),
                    message,
                )),
            }
        }
        let by_id: BTreeMap<RecordId, &KnowledgeRecord> =
            records.iter().map(|r| (r.id, r)).collect();
        let mut decisions = Vec::new();
        for id in &row.task.decisions {
            if let Some(record) = by_id.get(id) {
                decisions.push(memory_record(record, &[], Some(&pinned))?);
            }
        }
        let mut stale_knowledge = Vec::new();
        let mut seen = BTreeSet::new();
        for issue in &digest.decision_issues {
            if matches!(issue.problem, DecisionProblem::Stale { .. })
                && seen.insert(issue.record)
                && let Some(record) = by_id.get(&issue.record)
            {
                stale_knowledge.push(memory_record(record, &[], Some(&pinned))?);
            }
        }
        for stale in &digest.stale_in_changed_projects {
            if seen.insert(stale.record)
                && let Some(record) = by_id.get(&stale.record)
            {
                stale_knowledge.push(memory_record(record, &[], Some(&pinned))?);
            }
        }
        let last_manifest = checkpoints
            .iter()
            .max_by_key(|c| c.at)
            .map_or_else(|| row.task.view_manifest.clone(), |c| c.manifest.clone());
        let manifest = last_manifest
            .iter()
            .filter_map(|pin| {
                Some(knowell_mcp::ProjectView {
                    project: pin.project.clone(),
                    view: pin.view.as_str().parse().ok()?,
                    layer: if pin.local_generation.is_some() {
                        ViewLayer::Personal
                    } else {
                        ViewLayer::Shared
                    },
                    commit: CommitId::new(pin.commit.as_str()).ok(),
                    local_generation: pin.local_generation.unwrap_or(0),
                    freshness: None,
                    index_state: IndexState::Current,
                })
            })
            .collect();
        let mut mcp_checkpoints = Vec::new();
        for c in checkpoint_rows
            .iter()
            .rev()
            .take(usize::try_from(limit).unwrap_or(usize::MAX))
        {
            let author = row
                .task
                .notes
                .iter()
                .find(|n| n.at == c.checkpoint.at)
                .map_or_else(|| author_of(&Actor::System), |n| author_of(&n.author));
            mcp_checkpoints.push(Checkpoint {
                checkpoint_id: checkpoint_id(&row.task.id, c.seq)?,
                sequence: u32::try_from(c.seq).unwrap_or(u32::MAX),
                saved_at: mcp_time(c.checkpoint.at)?,
                author,
                progress: UntrustedText::memory(c.checkpoint.summary.clone()),
            });
        }
        Ok(ResumeTaskOutput {
            tasks: Vec::new(),
            task: Some(TaskDetail {
                summary: task_summary(&row, checkpoint_rows.last().map(|c| c.seq))?,
                checkpoints: mcp_checkpoints,
                decisions,
                open_questions: digest
                    .open_questions
                    .iter()
                    .map(|q| UntrustedText::memory(q.text.clone()))
                    .collect(),
                next_steps: digest
                    .next_steps
                    .iter()
                    .map(|s| UntrustedText::memory(s.clone()))
                    .collect(),
                related_symbols: row
                    .task
                    .related_symbols
                    .iter()
                    .map(|s| s.as_str().to_owned())
                    .collect(),
                manifest,
                changed_since,
                stale_knowledge,
            }),
            gaps,
        })
    }

    /// Files of `project` that changed between two commits (git tree diff,
    /// renames tracked), as project-relative paths.
    async fn changes_between(
        &self,
        pinned: &Pinned,
        project: &Name,
        from: &str,
        to: &str,
    ) -> Result<Vec<SourceChange>, String> {
        let entry = pinned
            .workspace
            .project(project)
            .ok_or_else(|| format!("{project} is not visible in this context"))?
            .clone();
        let (Ok(from_id), Ok(to_id)) = (CommitId::new(from), CommitId::new(to)) else {
            return Err("the recorded commits are not full commit ids".to_owned());
        };
        let path = entry.path.clone();
        let (from_s, to_s) = (from.to_owned(), to.to_owned());
        let changes = tokio::task::spawn_blocking(move || {
            let repo = GitRepo::open(&path).map_err(|e| e.to_string())?;
            repo.diff(&from_s, &to_s).map_err(|e| e.to_string())
        })
        .await
        .map_err(|e| format!("reading git history: {e}"))?
        .map_err(|_| format!("the commits of {project} could not be compared in its repository"))?;
        let root = entry.root.clone();
        let to_project = |p: &knowell_core::RepoPath| match &root {
            None => Some(p.clone()),
            Some(root) => p
                .as_str()
                .strip_prefix(root.as_str())
                .and_then(|r| r.strip_prefix('/'))
                .and_then(|r| knowell_core::RepoPath::new(r).ok()),
        };
        let mut out = Vec::new();
        for change in changes {
            let (path, kind, previous) = match &change {
                Change::Added(p) => (p, ChangeKind::Added, None),
                Change::Modified(p) => (p, ChangeKind::Modified, None),
                Change::Deleted(p) => (p, ChangeKind::Deleted, None),
                Change::Renamed { from, to, .. } => (to, ChangeKind::Renamed, to_project(from)),
            };
            let Some(path) = to_project(path) else {
                continue;
            };
            out.push(SourceChange {
                project: project.clone(),
                path,
                change: kind,
                previous_path: previous,
                from_commit: from_id.clone(),
                to_commit: to_id.clone(),
            });
        }
        Ok(out)
    }

    pub(crate) async fn tool_save_checkpoint(
        &self,
        access: Access,
        input: SaveCheckpointInput,
    ) -> Result<SaveCheckpointOutput, ToolError> {
        let pinned = self.resolve_target(&access, &input.target).await?;
        let ws = pinned.workspace.name.clone();
        if !access.allows(Action::WriteTask, &Resource::workspace(ws.clone())) {
            return Err(ToolError::permission_denied(
                "you may not write tasks in this workspace",
            ));
        }
        let key = input
            .idempotency_key
            .as_ref()
            .map(|k| format!("{}\u{0}{k}", access.label()));
        if let Some(key) = &key {
            let known = self
                .inner
                .checkpoint_keys
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .get(key)
                .copied();
            if let Some((task, seq)) = known {
                return self
                    .checkpoint_output(&access, &pinned, task, seq, false, false)
                    .await;
            }
        }
        let actor = access.actor()?;
        let now = now_timestamp();
        let (mut row, created_task) = match &input.task_id {
            Some(id) => (
                self.own_task(&access, &pinned, parse_task_id(id)?).await?,
                false,
            ),
            None => {
                let goal = input.goal.as_deref().unwrap_or_default();
                let title = input.title.clone().unwrap_or_else(|| {
                    goal.lines()
                        .next()
                        .unwrap_or("task")
                        .chars()
                        .take(120)
                        .collect()
                });
                let id = match &input.idempotency_key {
                    Some(k) => KTaskId::from_uuid(idempotent_uuid(
                        "knowell.engine.task.v1",
                        access.label(),
                        k,
                    )),
                    None => KTaskId::generate(),
                };
                if let Some(existing) =
                    self.inner.memory.get_task(id).await.map_err(memory_error)?
                {
                    (existing, false)
                } else {
                    let mut task = Task::new(id, &title, goal, now).map_err(knowledge_error)?;
                    task.set_status(KTaskStatus::InProgress, now)
                        .map_err(knowledge_error)?;
                    let row = TaskRow {
                        task,
                        workspace: Some(ws.clone()),
                        owner: access.acting_user().map(|u| u.to_string()),
                        revision: 0,
                    };
                    (
                        self.inner
                            .memory
                            .create_task(&row)
                            .await
                            .map_err(memory_error)?,
                        true,
                    )
                }
            }
        };
        if row.task.status.is_terminal() {
            return Err(ToolError::invalid_input(format!(
                "task {} is {} and cannot take checkpoints",
                row.task.id, row.task.status
            )));
        }
        let before = row.clone();
        let mut task = row.task.clone();
        let mut decision_records = Vec::new();
        for decision in &input.decisions {
            let evidence = self.evidence_from_ids(&pinned, &decision.evidence).await?;
            let outcome = KnowledgeRecord::write(
                NewRecord {
                    id: RecordId::generate(),
                    scope: Scope::Task(task.id),
                    kind: if access.is_agent() {
                        RecordKind::ModelSuggestion
                    } else {
                        RecordKind::Human
                    },
                    subject: subject_from(&decision.title, MemoryKind::Decision)?,
                    title: decision.title.clone(),
                    body: decision.body.clone(),
                    evidence,
                    related_symbols: Vec::new(),
                    tags: vec!["decision".to_owned()],
                    pinned: false,
                },
                actor.clone(),
                Rights::NONE,
                &self.inner.settings.acceptance,
                now,
            )
            .map_err(knowledge_error)?;
            let stored = self
                .inner
                .memory
                .insert_record(&outcome.record)
                .await
                .map_err(memory_error)?;
            task.add_decision(stored.record.id, now)
                .map_err(knowledge_error)?;
            decision_records.push(stored.record);
        }
        task.add_note(actor.clone(), &input.progress, now)
            .map_err(knowledge_error)?;
        if !input.open_questions.is_empty() {
            let wanted: BTreeSet<&str> = input.open_questions.iter().map(|q| q.trim()).collect();
            let open: Vec<(u32, String)> = task
                .open_questions
                .iter()
                .filter(|q| q.resolved_at.is_none())
                .map(|q| (q.id, q.text.clone()))
                .collect();
            for (id, text) in &open {
                if !wanted.contains(text.as_str()) {
                    task.resolve_question(*id, now).map_err(knowledge_error)?;
                }
            }
            for question in &input.open_questions {
                if !open.iter().any(|(_, t)| t == question.trim()) {
                    task.ask_question(question, now).map_err(knowledge_error)?;
                }
            }
        }
        if !input.related_symbols.is_empty() {
            let symbols = input
                .related_symbols
                .iter()
                .map(|s| {
                    SymbolId::new(s.trim()).map_err(|e| ToolError::invalid_input(e.to_string()))
                })
                .collect::<Result<Vec<_>, _>>()?;
            task.link(symbols, Vec::new(), now)
                .map_err(knowledge_error)?;
        }
        task.set_manifest(Self::manifest_pins(&pinned), now)
            .map_err(knowledge_error)?;
        if let Some(status) = input.status {
            let to = match status {
                TaskStatus::InProgress => KTaskStatus::InProgress,
                TaskStatus::Blocked => KTaskStatus::Blocked,
                TaskStatus::Done => KTaskStatus::Done,
                TaskStatus::Abandoned => KTaskStatus::Abandoned,
            };
            if task.status != to {
                task.set_status(to, now).map_err(knowledge_error)?;
            }
        }
        let checkpoint = task
            .checkpoint(&input.progress, input.next_steps.clone(), now)
            .map_err(knowledge_error)?;
        row = self
            .inner
            .memory
            .update_task(&before, &task)
            .await
            .map_err(memory_error)?;
        let seq = self
            .inner
            .memory
            .append_checkpoint(&checkpoint)
            .await
            .map_err(memory_error)?;
        if let Some(key) = key {
            self.inner
                .checkpoint_keys
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .insert(key, (row.task.id, seq));
        }
        let mut out = self
            .checkpoint_output(&access, &pinned, row.task.id, seq, created_task, true)
            .await?;
        for record in &decision_records {
            out.decisions
                .push(memory_record(record, &[], Some(&pinned))?);
        }
        Ok(out)
    }

    async fn checkpoint_output(
        &self,
        access: &Access,
        pinned: &Pinned,
        task: KTaskId,
        seq: u64,
        created_task: bool,
        created: bool,
    ) -> Result<SaveCheckpointOutput, ToolError> {
        let row = self.own_task(access, pinned, task).await?;
        let checkpoints = self
            .inner
            .memory
            .checkpoints(task)
            .await
            .map_err(memory_error)?;
        let saved = checkpoints
            .iter()
            .find(|c| c.seq == seq)
            .ok_or_else(|| ToolError::internal("checkpoint vanished"))?;
        Ok(SaveCheckpointOutput {
            task_id: TaskId::new(task.to_string())
                .map_err(|e| ToolError::internal(e.to_string()))?,
            checkpoint_id: checkpoint_id(&task, seq)?,
            sequence: u32::try_from(seq).unwrap_or(u32::MAX),
            saved_at: mcp_time(saved.checkpoint.at)?,
            created_task,
            created,
            manifest: pinned.manifest(),
            decisions: if created {
                Vec::new()
            } else {
                let mut out = Vec::new();
                for id in &row.task.decisions {
                    if let Some(r) = self
                        .inner
                        .memory
                        .get_record(*id)
                        .await
                        .map_err(memory_error)?
                    {
                        out.push(memory_record(&r.record, &[], Some(pinned))?);
                    }
                }
                out
            },
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn subjects_come_from_titles() {
        assert_eq!(
            subject_from("Payments: idempotency keys!", MemoryKind::Decision)
                .unwrap()
                .as_str(),
            "payments.idempotency.keys"
        );
        assert_eq!(
            subject_from("Ödeme çift çekim", MemoryKind::Note)
                .unwrap()
                .as_str(),
            "deme.ift.ekim"
        );
        assert_eq!(
            subject_from("!!!", MemoryKind::Rule).unwrap().as_str(),
            "rule"
        );
    }

    #[test]
    fn idempotent_ids_depend_on_owner_and_key() {
        let a = idempotent_uuid("n", "alice", "k1");
        assert_eq!(a, idempotent_uuid("n", "alice", "k1"));
        assert_ne!(a, idempotent_uuid("n", "bob", "k1"));
        assert_ne!(a, idempotent_uuid("n", "alice", "k2"));
    }

    #[test]
    fn kinds_round_trip_through_tags() {
        for kind in [
            MemoryKind::Decision,
            MemoryKind::Rule,
            MemoryKind::Example,
            MemoryKind::Finding,
            MemoryKind::Note,
            MemoryKind::Observation,
            MemoryKind::Description,
        ] {
            let record = KnowledgeRecord::propose(
                NewRecord {
                    id: RecordId::generate(),
                    scope: Scope::Organization,
                    kind: RecordKind::ModelSuggestion,
                    subject: Subject::new("a").unwrap(),
                    title: "t".into(),
                    body: "b".into(),
                    evidence: Vec::new(),
                    related_symbols: Vec::new(),
                    tags: vec![kind_tag(kind).into()],
                    pinned: false,
                },
                Actor::System,
                KTimestamp::from_unix_seconds(0),
            )
            .unwrap();
            assert_eq!(kind_of(&record), kind);
        }
    }

    #[test]
    fn timestamps_are_rfc3339_utc() {
        let t = mcp_time(KTimestamp::from_unix_seconds(1_790_000_000)).unwrap();
        assert!(t.as_str().ends_with('Z'), "{}", t.as_str());
    }
}
