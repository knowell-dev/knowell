//! [`MemoryRepo`] over the `knowell-store` knowledge and task tables.
//!
//! The store references workspaces, projects and tasks by id; the domain
//! model by name. A [`Directory`] shared with the engine's registry maps one
//! to the other. Evidence names a project without its workspace: it is
//! resolved inside the record's workspace when the scope has one, otherwise
//! to the only registered project of that name (ambiguous names are
//! rejected, never guessed).

use std::collections::BTreeMap;
use std::sync::{Arc, PoisonError, RwLock};

use knowell_core::Name;
use knowell_knowledge::{
    Action, Actor, Checkpoint, CommitId, Evidence, FileRef, HistoryEntry, KnowledgeRecord,
    ManifestPin, OpenQuestion, ProgressNote, RecordId, RecordKind, RecordState, Scope, Subject,
    SymbolId, Task, TaskId, TaskStatus, Timestamp, UserId, ViewId,
};
use knowell_store::knowledge::{
    self, NewRecord, RecordContent, RecordEvidence, RecordFilter, RecordScope, RecordUpdate,
    StoredRecord,
};
use knowell_store::tasks::{
    self, NewCheckpoint, NewTask, StoredTask, TaskDetails, TaskFilter, TaskUpdate,
};
use knowell_store::{
    KnowledgeAction, KnowledgeKind, KnowledgeRecordId, KnowledgeState, OrganizationId, ProjectId,
    Store, StoreError, WorkspaceId,
};
use time::OffsetDateTime;

use super::{BoxFuture, CheckpointRow, MemoryError, MemoryRepo, RecordQuery, RecordRow, TaskRow};

/// Name ↔ id maps of the registered workspaces and projects.
#[derive(Debug, Default)]
pub(crate) struct Directory {
    workspaces: BTreeMap<Name, WorkspaceId>,
    workspace_names: BTreeMap<WorkspaceId, Name>,
    projects: BTreeMap<(Name, Name), ProjectId>,
    project_names: BTreeMap<ProjectId, (Name, Name)>,
}

impl Directory {
    /// Registers a workspace and its projects.
    pub(crate) fn add_workspace(
        &mut self,
        name: &Name,
        id: WorkspaceId,
        projects: impl IntoIterator<Item = (Name, ProjectId)>,
    ) {
        self.workspaces.insert(name.clone(), id);
        self.workspace_names.insert(id, name.clone());
        for (project, project_id) in projects {
            self.projects
                .insert((name.clone(), project.clone()), project_id);
            self.project_names
                .insert(project_id, (name.clone(), project));
        }
    }

    fn workspace(&self, name: &Name) -> Result<WorkspaceId, MemoryError> {
        self.workspaces
            .get(name)
            .copied()
            .ok_or_else(|| MemoryError::Invalid(format!("workspace {name} is not registered")))
    }

    fn project(&self, workspace: &Name, project: &Name) -> Result<ProjectId, MemoryError> {
        self.projects
            .get(&(workspace.clone(), project.clone()))
            .copied()
            .ok_or_else(|| {
                MemoryError::Invalid(format!("project {workspace}/{project} is not registered"))
            })
    }

    /// The project of evidence: inside `workspace` when known, else the
    /// only registered project with that name.
    fn evidence_project(
        &self,
        workspace: Option<&Name>,
        project: &Name,
    ) -> Result<ProjectId, MemoryError> {
        if let Some(workspace) = workspace {
            return self.project(workspace, project);
        }
        let mut found = self
            .projects
            .iter()
            .filter(|((_, p), _)| p == project)
            .map(|(_, id)| *id);
        match (found.next(), found.next()) {
            (Some(id), None) => Ok(id),
            (None, _) => Err(MemoryError::Invalid(format!(
                "evidence names project {project}, which is not registered"
            ))),
            (Some(_), Some(_)) => Err(MemoryError::Invalid(format!(
                "evidence names project {project}, which exists in several workspaces; use a workspace or project scope"
            ))),
        }
    }
}

/// Memory records and tasks in PostgreSQL.
#[derive(Debug, Clone)]
pub struct StoreMemory {
    store: Store,
    organization: OrganizationId,
    directory: Arc<RwLock<Directory>>,
}

impl StoreMemory {
    pub(crate) fn new(
        store: Store,
        organization: OrganizationId,
        directory: Arc<RwLock<Directory>>,
    ) -> Self {
        Self {
            store,
            organization,
            directory,
        }
    }

    fn with_directory<T>(&self, f: impl FnOnce(&Directory) -> T) -> T {
        let directory = self
            .directory
            .read()
            .unwrap_or_else(PoisonError::into_inner);
        f(&directory)
    }
}

fn store_error(error: StoreError) -> MemoryError {
    match error {
        StoreError::AlreadyExists { entity, key } => {
            MemoryError::AlreadyExists(format!("{entity} {key}"))
        }
        StoreError::NotFound { entity, key } => MemoryError::NotFound(format!("{entity} {key}")),
        StoreError::Conflict { entity, key, .. } => {
            MemoryError::Conflict(format!("{entity} {key} changed since it was read"))
        }
        other => MemoryError::Store(other),
    }
}

fn to_datetime(at: Timestamp) -> Result<OffsetDateTime, MemoryError> {
    at.to_datetime()
        .ok_or_else(|| MemoryError::Invalid("timestamp out of range".to_owned()))
}

fn kind_to_store(kind: RecordKind) -> KnowledgeKind {
    match kind {
        RecordKind::Observed => KnowledgeKind::Observed,
        RecordKind::Human => KnowledgeKind::Human,
        RecordKind::ModelSuggestion => KnowledgeKind::ModelSuggestion,
    }
}

fn kind_from_store(kind: KnowledgeKind) -> RecordKind {
    match kind {
        KnowledgeKind::Observed => RecordKind::Observed,
        KnowledgeKind::Human => RecordKind::Human,
        KnowledgeKind::ModelSuggestion => RecordKind::ModelSuggestion,
    }
}

fn state_to_store(state: RecordState) -> KnowledgeState {
    match state {
        RecordState::Proposed => KnowledgeState::Proposed,
        RecordState::Accepted => KnowledgeState::Accepted,
        RecordState::Rejected => KnowledgeState::Rejected,
        RecordState::Stale => KnowledgeState::Stale,
        RecordState::Superseded => KnowledgeState::Superseded,
    }
}

fn state_from_store(state: KnowledgeState) -> RecordState {
    match state {
        KnowledgeState::Proposed => RecordState::Proposed,
        KnowledgeState::Accepted => RecordState::Accepted,
        KnowledgeState::Rejected => RecordState::Rejected,
        KnowledgeState::Stale => RecordState::Stale,
        KnowledgeState::Superseded => RecordState::Superseded,
    }
}

fn action_to_store(action: Action) -> KnowledgeAction {
    match action {
        Action::Propose => KnowledgeAction::Propose,
        Action::Accept => KnowledgeAction::Accept,
        Action::Reject => KnowledgeAction::Reject,
        Action::MarkStale => KnowledgeAction::MarkStale,
        Action::Revalidate => KnowledgeAction::Revalidate,
        Action::Supersede => KnowledgeAction::Supersede,
        Action::Edit => KnowledgeAction::Edit,
        Action::Pin => KnowledgeAction::Pin,
    }
}

fn action_from_store(action: KnowledgeAction) -> Action {
    match action {
        KnowledgeAction::Propose => Action::Propose,
        KnowledgeAction::Accept => Action::Accept,
        KnowledgeAction::Reject => Action::Reject,
        KnowledgeAction::MarkStale => Action::MarkStale,
        KnowledgeAction::Revalidate => Action::Revalidate,
        KnowledgeAction::Supersede => Action::Supersede,
        KnowledgeAction::Edit => Action::Edit,
        KnowledgeAction::Pin => Action::Pin,
    }
}

fn status_to_store(status: TaskStatus) -> knowell_store::TaskStatus {
    match status {
        TaskStatus::Open => knowell_store::TaskStatus::Open,
        TaskStatus::InProgress => knowell_store::TaskStatus::InProgress,
        TaskStatus::Blocked => knowell_store::TaskStatus::Blocked,
        TaskStatus::Done => knowell_store::TaskStatus::Done,
        TaskStatus::Abandoned => knowell_store::TaskStatus::Abandoned,
    }
}

fn status_from_store(status: knowell_store::TaskStatus) -> TaskStatus {
    match status {
        knowell_store::TaskStatus::Open => TaskStatus::Open,
        knowell_store::TaskStatus::InProgress => TaskStatus::InProgress,
        knowell_store::TaskStatus::Blocked => TaskStatus::Blocked,
        knowell_store::TaskStatus::Done => TaskStatus::Done,
        knowell_store::TaskStatus::Abandoned => TaskStatus::Abandoned,
    }
}

fn record_id(id: RecordId) -> KnowledgeRecordId {
    KnowledgeRecordId(id.as_uuid())
}

fn store_task_id(id: TaskId) -> knowell_store::TaskId {
    knowell_store::TaskId(id.as_uuid())
}

fn json<T: serde::Serialize>(value: &T) -> Result<serde_json::Value, MemoryError> {
    serde_json::to_value(value).map_err(|e| MemoryError::Invalid(format!("serialising: {e}")))
}

fn from_json<T: serde::de::DeserializeOwned>(
    value: serde_json::Value,
    what: &str,
) -> Result<T, MemoryError> {
    serde_json::from_value(value).map_err(|e| MemoryError::Invalid(format!("stored {what}: {e}")))
}

impl StoreMemory {
    fn scope_to_store(&self, scope: &Scope) -> Result<RecordScope, MemoryError> {
        self.with_directory(|d| {
            Ok(match scope {
                Scope::Organization => RecordScope::Organization,
                Scope::Workspace(name) => RecordScope::Workspace(d.workspace(name)?),
                Scope::Project { workspace, project } => {
                    RecordScope::Project(d.project(workspace, project)?)
                }
                Scope::Task(id) => RecordScope::Task(store_task_id(*id)),
                Scope::User(user) => RecordScope::User(user.as_str().to_owned()),
            })
        })
    }

    fn scope_from_store(&self, scope: &RecordScope) -> Result<Scope, MemoryError> {
        self.with_directory(|d| {
            Ok(match scope {
                RecordScope::Organization => Scope::Organization,
                RecordScope::Workspace(id) => {
                    Scope::Workspace(d.workspace_names.get(id).cloned().ok_or_else(|| {
                        MemoryError::Invalid(format!("workspace {id} is not registered"))
                    })?)
                }
                RecordScope::Project(id) => {
                    let (workspace, project) =
                        d.project_names.get(id).cloned().ok_or_else(|| {
                            MemoryError::Invalid(format!("project {id} is not registered"))
                        })?;
                    Scope::Project { workspace, project }
                }
                RecordScope::Task(id) => Scope::Task(TaskId::from_uuid(id.0)),
                RecordScope::User(key) => Scope::User(
                    UserId::new(key.clone())
                        .map_err(|e| MemoryError::Invalid(format!("stored user key: {e}")))?,
                ),
            })
        })
    }

    fn scope_workspace(scope: &Scope) -> Option<&Name> {
        match scope {
            Scope::Workspace(w) | Scope::Project { workspace: w, .. } => Some(w),
            _ => None,
        }
    }

    fn evidence_to_store(
        &self,
        scope: &Scope,
        evidence: &[Evidence],
    ) -> Result<Vec<RecordEvidence>, MemoryError> {
        let workspace = Self::scope_workspace(scope);
        self.with_directory(|d| {
            evidence
                .iter()
                .map(|e| {
                    Ok(RecordEvidence {
                        project: d.evidence_project(workspace, &e.project)?,
                        view: e.view.as_str().to_owned(),
                        commit: e.commit.as_str().to_owned(),
                        path: e.path.clone(),
                        lines: e.range,
                        content_hash: e.content_hash,
                    })
                })
                .collect()
        })
    }

    fn evidence_from_store(
        &self,
        evidence: &[RecordEvidence],
    ) -> Result<Vec<Evidence>, MemoryError> {
        self.with_directory(|d| {
            evidence
                .iter()
                .map(|e| {
                    let (_, project) =
                        d.project_names.get(&e.project).cloned().ok_or_else(|| {
                            MemoryError::Invalid(format!(
                                "evidence project {} is not registered",
                                e.project
                            ))
                        })?;
                    Ok(Evidence {
                        project,
                        view: ViewId::new(e.view.clone())
                            .map_err(|err| MemoryError::Invalid(format!("stored view: {err}")))?,
                        commit: CommitId::new(e.commit.clone())
                            .map_err(|err| MemoryError::Invalid(format!("stored commit: {err}")))?,
                        path: e.path.clone(),
                        range: e.lines,
                        content_hash: e.content_hash,
                    })
                })
                .collect()
        })
    }

    fn history_to_store(
        entries: &[HistoryEntry],
    ) -> Result<Vec<knowledge::HistoryEntry>, MemoryError> {
        entries
            .iter()
            .map(|h| {
                Ok(knowledge::HistoryEntry {
                    at: to_datetime(h.at)?,
                    actor: json(&h.actor)?,
                    action: action_to_store(h.action),
                    from: h.from.map(state_to_store),
                    to: state_to_store(h.to),
                    version: h.version,
                    reason: h.reason.clone(),
                })
            })
            .collect()
    }

    fn record_to_store(&self, record: &KnowledgeRecord) -> Result<NewRecord, MemoryError> {
        Ok(NewRecord {
            id: record_id(record.id),
            organization: self.organization,
            scope: self.scope_to_store(&record.scope)?,
            kind: kind_to_store(record.kind),
            subject: record.subject.as_str().to_owned(),
            title: record.title.clone(),
            body: record.body.clone(),
            state: state_to_store(record.state),
            version: record.version,
            author: json(&record.author)?,
            pinned: record.pinned,
            tags: record.tags.clone(),
            related_symbols: record
                .related_symbols
                .iter()
                .map(|s| s.as_str().to_owned())
                .collect(),
            superseded_by: record.superseded_by.map(record_id),
            evidence: self.evidence_to_store(&record.scope, &record.evidence)?,
            history: Self::history_to_store(&record.history)?,
            created_at: to_datetime(record.created_at)?,
            updated_at: to_datetime(record.updated_at)?,
        })
    }

    async fn record_from_store(&self, stored: StoredRecord) -> Result<RecordRow, MemoryError> {
        let mut conn = self.store.acquire().await.map_err(store_error)?;
        let history = knowledge::record_history(&mut conn, stored.id)
            .await
            .map_err(store_error)?;
        drop(conn);
        let history = history
            .into_iter()
            .map(|h| {
                Ok(HistoryEntry {
                    at: Timestamp::from_datetime(h.at),
                    actor: from_json(h.actor, "actor")?,
                    action: action_from_store(h.action),
                    from: h.from.map(state_from_store),
                    to: state_from_store(h.to),
                    version: h.version,
                    reason: h.reason,
                })
            })
            .collect::<Result<Vec<_>, MemoryError>>()?;
        let author: Actor = from_json(stored.author.clone(), "author")?;
        let record = KnowledgeRecord {
            id: RecordId::from_uuid(stored.id.0),
            scope: self.scope_from_store(&stored.scope)?,
            kind: kind_from_store(stored.kind),
            subject: Subject::new(&stored.subject)
                .map_err(|e| MemoryError::Invalid(format!("stored subject: {e}")))?,
            title: stored.title,
            body: stored.body,
            state: state_from_store(stored.state),
            version: stored.version,
            author,
            created_at: Timestamp::from_datetime(stored.created_at),
            updated_at: Timestamp::from_datetime(stored.updated_at),
            evidence: self.evidence_from_store(&stored.evidence)?,
            related_symbols: stored
                .related_symbols
                .iter()
                .map(|s| {
                    SymbolId::new(s.clone())
                        .map_err(|e| MemoryError::Invalid(format!("stored symbol: {e}")))
                })
                .collect::<Result<Vec<_>, _>>()?,
            tags: stored.tags,
            pinned: stored.pinned,
            superseded_by: stored.superseded_by.map(|id| RecordId::from_uuid(id.0)),
            previous_versions: Vec::new(),
            history,
        };
        Ok(RecordRow {
            record,
            revision: stored.revision,
        })
    }

    async fn rows_from_store(
        &self,
        stored: Vec<StoredRecord>,
    ) -> Result<Vec<RecordRow>, MemoryError> {
        let mut rows = Vec::with_capacity(stored.len());
        for record in stored {
            match self.record_from_store(record).await {
                Ok(row) => rows.push(row),
                // Records of projects this engine does not serve are skipped,
                // not fatal for the whole listing.
                Err(MemoryError::Invalid(reason)) => {
                    tracing::debug!(%reason, "skipping a memory record the engine cannot map");
                }
                Err(other) => return Err(other),
            }
        }
        Ok(rows)
    }

    fn task_to_details(&self, task: &Task) -> Result<TaskDetails, MemoryError> {
        Ok(TaskDetails {
            notes: json(&task.notes)?,
            decisions: task.decisions.iter().copied().map(record_id).collect(),
            open_questions: json(&task.open_questions)?,
            related_symbols: task
                .related_symbols
                .iter()
                .map(|s| s.as_str().to_owned())
                .collect(),
            related_files: json(&task.related_files)?,
            view_manifest: json(&task.view_manifest)?,
        })
    }

    fn task_from_store(&self, stored: StoredTask) -> Result<TaskRow, MemoryError> {
        let workspace = match stored.workspace {
            Some(id) => self.with_directory(|d| d.workspace_names.get(&id).cloned()),
            None => None,
        };
        let details = stored.details;
        let notes: Vec<ProgressNote> = from_json(details.notes, "task notes")?;
        let open_questions: Vec<OpenQuestion> =
            from_json(details.open_questions, "task questions")?;
        let related_files: Vec<FileRef> = from_json(details.related_files, "task files")?;
        let view_manifest: Vec<ManifestPin> = from_json(details.view_manifest, "task manifest")?;
        let task = Task {
            id: TaskId::from_uuid(stored.id.0),
            title: stored.title,
            goal: stored.goal,
            status: status_from_store(stored.status),
            created_at: Timestamp::from_datetime(stored.created_at),
            updated_at: Timestamp::from_datetime(stored.updated_at),
            notes,
            decisions: details
                .decisions
                .iter()
                .map(|id| RecordId::from_uuid(id.0))
                .collect(),
            open_questions,
            related_symbols: details
                .related_symbols
                .iter()
                .map(|s| {
                    SymbolId::new(s.clone())
                        .map_err(|e| MemoryError::Invalid(format!("stored symbol: {e}")))
                })
                .collect::<Result<Vec<_>, _>>()?,
            related_files,
            view_manifest,
        };
        Ok(TaskRow {
            task,
            workspace,
            owner: stored.owner,
            revision: stored.revision,
        })
    }
}

impl MemoryRepo for StoreMemory {
    fn insert_record<'a>(
        &'a self,
        record: &'a KnowledgeRecord,
    ) -> BoxFuture<'a, Result<RecordRow, MemoryError>> {
        Box::pin(async move {
            let new = self.record_to_store(record)?;
            let mut conn = self.store.acquire().await.map_err(store_error)?;
            let stored = knowledge::insert_record(&mut conn, &new)
                .await
                .map_err(store_error)?;
            drop(conn);
            self.record_from_store(stored).await
        })
    }

    fn update_record<'a>(
        &'a self,
        before: &'a RecordRow,
        after: &'a KnowledgeRecord,
    ) -> BoxFuture<'a, Result<RecordRow, MemoryError>> {
        Box::pin(async move {
            let appended = after
                .history
                .get(before.record.history.len()..)
                .unwrap_or_default();
            let content = if after.version != before.record.version {
                Some(RecordContent {
                    title: after.title.clone(),
                    body: after.body.clone(),
                    tags: after.tags.clone(),
                    evidence: self.evidence_to_store(&after.scope, &after.evidence)?,
                })
            } else {
                None
            };
            let update = RecordUpdate {
                id: record_id(after.id),
                expected_version: before.record.version,
                expected_revision: before.revision,
                state: state_to_store(after.state),
                pinned: after.pinned,
                related_symbols: after
                    .related_symbols
                    .iter()
                    .map(|s| s.as_str().to_owned())
                    .collect(),
                superseded_by: after.superseded_by.map(record_id),
                content,
                history: Self::history_to_store(appended)?,
                updated_at: to_datetime(after.updated_at)?,
            };
            let mut conn = self.store.acquire().await.map_err(store_error)?;
            let stored = knowledge::update_record(&mut conn, &update)
                .await
                .map_err(store_error)?;
            drop(conn);
            self.record_from_store(stored).await
        })
    }

    fn get_record(&self, id: RecordId) -> BoxFuture<'_, Result<Option<RecordRow>, MemoryError>> {
        Box::pin(async move {
            let mut conn = self.store.acquire().await.map_err(store_error)?;
            let stored = knowledge::get_record(&mut conn, record_id(id))
                .await
                .map_err(store_error)?;
            drop(conn);
            match stored {
                Some(stored) if stored.organization == self.organization => {
                    Ok(Some(self.record_from_store(stored).await?))
                }
                _ => Ok(None),
            }
        })
    }

    fn find_records<'a>(
        &'a self,
        query: &'a RecordQuery,
    ) -> BoxFuture<'a, Result<Vec<RecordRow>, MemoryError>> {
        Box::pin(async move {
            let mut scopes = Vec::with_capacity(query.scopes.len());
            for scope in &query.scopes {
                match self.scope_to_store(scope) {
                    Ok(s) => scopes.push(s),
                    // A scope the store cannot name holds no records.
                    Err(MemoryError::Invalid(_)) => {}
                    Err(other) => return Err(other),
                }
            }
            if !query.scopes.is_empty() && scopes.is_empty() {
                return Ok(Vec::new());
            }
            let limit = query.limit.clamp(1, knowledge::MAX_RECORDS_LISTED);
            let filter = RecordFilter {
                organization: self.organization,
                scopes,
                states: query.states.iter().copied().map(state_to_store).collect(),
                kinds: query.kinds.iter().copied().map(kind_to_store).collect(),
                subject: None,
                pinned: None,
                tag: None,
                limit,
                before: None,
            };
            let mut conn = self.store.acquire().await.map_err(store_error)?;
            let stored = match query
                .text
                .as_deref()
                .map(str::trim)
                .filter(|t| !t.is_empty())
            {
                Some(text) => {
                    let text: String = text.chars().take(knowledge::MAX_QUERY_BYTES / 4).collect();
                    knowledge::search_records(&mut conn, &filter, &text)
                        .await
                        .map_err(store_error)?
                        .into_iter()
                        .map(|hit| hit.record)
                        .collect()
                }
                None => knowledge::list_records(&mut conn, &filter)
                    .await
                    .map_err(store_error)?,
            };
            drop(conn);
            self.rows_from_store(stored).await
        })
    }

    fn create_task<'a>(&'a self, row: &'a TaskRow) -> BoxFuture<'a, Result<TaskRow, MemoryError>> {
        Box::pin(async move {
            let workspace = match &row.workspace {
                Some(name) => Some(self.with_directory(|d| d.workspace(name))?),
                None => None,
            };
            let new = NewTask {
                id: store_task_id(row.task.id),
                organization: self.organization,
                workspace,
                owner: row.owner.clone(),
                title: row.task.title.clone(),
                goal: row.task.goal.clone(),
                status: status_to_store(row.task.status),
                details: self.task_to_details(&row.task)?,
                created_at: to_datetime(row.task.created_at)?,
                updated_at: to_datetime(row.task.updated_at)?,
            };
            let mut conn = self.store.acquire().await.map_err(store_error)?;
            let stored = tasks::create_task(&mut conn, &new)
                .await
                .map_err(store_error)?;
            self.task_from_store(stored)
        })
    }

    fn get_task(&self, id: TaskId) -> BoxFuture<'_, Result<Option<TaskRow>, MemoryError>> {
        Box::pin(async move {
            let mut conn = self.store.acquire().await.map_err(store_error)?;
            let stored = tasks::get_task(&mut conn, store_task_id(id))
                .await
                .map_err(store_error)?;
            match stored {
                Some(stored) if stored.organization == self.organization => {
                    Ok(Some(self.task_from_store(stored)?))
                }
                _ => Ok(None),
            }
        })
    }

    fn list_tasks<'a>(
        &'a self,
        workspace: Option<&'a Name>,
        statuses: &'a [TaskStatus],
        limit: u32,
    ) -> BoxFuture<'a, Result<Vec<TaskRow>, MemoryError>> {
        Box::pin(async move {
            let workspace = match workspace {
                Some(name) => Some(self.with_directory(|d| d.workspace(name))?),
                None => None,
            };
            let filter = TaskFilter {
                organization: self.organization,
                workspace,
                owner: None,
                statuses: statuses.iter().copied().map(status_to_store).collect(),
                limit: limit.clamp(1, tasks::MAX_TASKS_LISTED),
                before: None,
            };
            let mut conn = self.store.acquire().await.map_err(store_error)?;
            let stored = tasks::list_tasks(&mut conn, &filter)
                .await
                .map_err(store_error)?;
            stored
                .into_iter()
                .map(|t| self.task_from_store(t))
                .collect()
        })
    }

    fn update_task<'a>(
        &'a self,
        before: &'a TaskRow,
        after: &'a Task,
    ) -> BoxFuture<'a, Result<TaskRow, MemoryError>> {
        Box::pin(async move {
            let update = TaskUpdate {
                id: store_task_id(after.id),
                expected_revision: before.revision,
                title: after.title.clone(),
                goal: after.goal.clone(),
                status: status_to_store(after.status),
                details: self.task_to_details(after)?,
                updated_at: to_datetime(after.updated_at)?,
            };
            let mut conn = self.store.acquire().await.map_err(store_error)?;
            let stored = tasks::update_task(&mut conn, &update)
                .await
                .map_err(store_error)?;
            self.task_from_store(stored)
        })
    }

    fn append_checkpoint<'a>(
        &'a self,
        checkpoint: &'a Checkpoint,
    ) -> BoxFuture<'a, Result<u64, MemoryError>> {
        Box::pin(async move {
            let new = NewCheckpoint {
                task: store_task_id(checkpoint.task),
                at: to_datetime(checkpoint.at)?,
                summary: checkpoint.summary.clone(),
                decisions: checkpoint
                    .decisions
                    .iter()
                    .copied()
                    .map(record_id)
                    .collect(),
                next_steps: checkpoint.next_steps.clone(),
                manifest: json(&checkpoint.manifest)?,
            };
            let mut conn = self.store.acquire().await.map_err(store_error)?;
            let stored = tasks::append_checkpoint(&mut conn, &new)
                .await
                .map_err(store_error)?;
            Ok(stored.seq)
        })
    }

    fn checkpoints(&self, task: TaskId) -> BoxFuture<'_, Result<Vec<CheckpointRow>, MemoryError>> {
        Box::pin(async move {
            let mut conn = self.store.acquire().await.map_err(store_error)?;
            let stored = tasks::list_checkpoints(&mut conn, store_task_id(task))
                .await
                .map_err(store_error)?;
            stored
                .into_iter()
                .map(|c| {
                    Ok(CheckpointRow {
                        seq: c.seq,
                        checkpoint: Checkpoint {
                            task,
                            at: Timestamp::from_datetime(c.at),
                            summary: c.summary,
                            decisions: c
                                .decisions
                                .iter()
                                .map(|id| RecordId::from_uuid(id.0))
                                .collect(),
                            next_steps: c.next_steps,
                            manifest: from_json(c.manifest, "checkpoint manifest")?,
                        },
                    })
                })
                .collect()
        })
    }
}
