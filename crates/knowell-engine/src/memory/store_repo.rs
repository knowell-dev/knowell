//! [`MemoryRepo`] over the `knowell-store` knowledge and task tables.
//!
//! The store references workspaces, projects and tasks by id; the domain
//! model by name. A [`Directory`] shared with the engine's registry maps one
//! to the other. Contextual writes retain the workspace in which evidence
//! was resolved. Legacy writes use the record's workspace when its scope has
//! one, otherwise the only registered namesake (ambiguous names are rejected).
//! Updates preserve canonical project ids instead of resolving saved names anew.

use std::collections::BTreeMap;
use std::sync::{Arc, PoisonError, RwLock};

use knowell_core::Name;
use knowell_knowledge::{
    Action, Actor, Checkpoint, CommitId, Evidence, FileRef, HistoryEntry, KnowledgeRecord,
    ManifestPin, OpenQuestion, ProgressNote, RecordId, RecordKind, RecordState, Scope, Subject,
    SymbolId, Task, TaskId, TaskStatus, Timestamp, UserId, ViewId,
};
use knowell_store::hierarchy;
use knowell_store::knowledge::{
    self, NewRecord, RecordContent, RecordEvidence, RecordFilter, RecordScope, RecordUpdate,
    StoredRecord,
};
use knowell_store::tasks::{
    self, CheckpointReceipt, CheckpointWrite, NewCheckpoint, NewTask, StoredTask, TaskDetails,
    TaskFilter, TaskUpdate,
};
use knowell_store::{
    CheckpointReceiptId, KnowledgeAction, KnowledgeKind, KnowledgeRecordId, KnowledgeState,
    OrganizationId, ProjectId, Store, StoreError, WorkspaceId,
};
use time::OffsetDateTime;
use uuid::Uuid;

use super::{
    BoxFuture, CheckpointRow, CheckpointSave, CheckpointSaved, MemoryError, MemoryRepo,
    RecordQuery, RecordRow, TaskRow,
};

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

fn stored_evidence(project: ProjectId, evidence: &Evidence) -> RecordEvidence {
    RecordEvidence {
        project,
        view: evidence.view.as_str().to_owned(),
        commit: evidence.commit.as_str().to_owned(),
        path: evidence.path.clone(),
        lines: evidence.range,
        content_hash: evidence.content_hash,
    }
}

// The caller verifies `before` against the current stored row before any ids
// are reused. Equal names and pointer bytes cannot distinguish two namespaces.
fn updated_evidence(
    directory: &Directory,
    before: &[Evidence],
    after: &[Evidence],
    stored: &[RecordEvidence],
    workspace: Option<&Name>,
) -> Result<Vec<RecordEvidence>, MemoryError> {
    if before.len() != stored.len() {
        return Err(MemoryError::Invalid(
            "saved memory evidence identity is incomplete".to_owned(),
        ));
    }
    if before == after {
        return Ok(stored.to_vec());
    }
    after
        .iter()
        .map(|evidence| {
            let mut retained: Option<&RecordEvidence> = None;
            for (previous, canonical) in before.iter().zip(stored) {
                if previous == evidence {
                    if retained.is_some_and(|known| known.project != canonical.project) {
                        return Err(MemoryError::Invalid(
                            "updated memory evidence has ambiguous saved identity".to_owned(),
                        ));
                    }
                    retained = Some(canonical);
                }
            }
            match retained {
                Some(canonical) => Ok(canonical.clone()),
                None => {
                    let workspace = workspace.ok_or_else(|| {
                        MemoryError::Invalid(
                            "updated memory evidence requires a verified workspace".to_owned(),
                        )
                    })?;
                    Ok(stored_evidence(
                        directory.project(workspace, &evidence.project)?,
                        evidence,
                    ))
                }
            }
        })
        .collect()
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
        evidence: &[Evidence],
        workspace: Option<&Name>,
    ) -> Result<Vec<RecordEvidence>, MemoryError> {
        self.with_directory(|d| {
            evidence
                .iter()
                .map(|e| {
                    Ok(stored_evidence(
                        d.evidence_project(workspace, &e.project)?,
                        e,
                    ))
                })
                .collect()
        })
    }

    async fn stored_workspace_name(&self, id: WorkspaceId) -> Result<Name, MemoryError> {
        if let Some(name) =
            self.with_directory(|directory| directory.workspace_names.get(&id).cloned())
        {
            return Ok(name);
        }
        let mut conn = self.store.acquire().await.map_err(store_error)?;
        hierarchy::get_workspace(&mut conn, id)
            .await
            .map_err(store_error)?
            .filter(|workspace| workspace.organization == self.organization)
            .map(|workspace| workspace.name)
            .ok_or_else(|| {
                MemoryError::NotFound("saved workspace metadata is unavailable".to_owned())
            })
    }

    async fn stored_project_names(&self, id: ProjectId) -> Result<(Name, Name), MemoryError> {
        if let Some(names) =
            self.with_directory(|directory| directory.project_names.get(&id).cloned())
        {
            return Ok(names);
        }
        let mut conn = self.store.acquire().await.map_err(store_error)?;
        let project = hierarchy::get_project(&mut conn, id)
            .await
            .map_err(store_error)?
            .filter(|project| project.organization == self.organization)
            .ok_or_else(|| {
                MemoryError::NotFound("saved source metadata is unavailable".to_owned())
            })?;
        drop(conn);
        let workspace = self.stored_workspace_name(project.workspace).await?;
        Ok((workspace, project.name))
    }

    async fn stored_scope(&self, scope: &RecordScope) -> Result<Scope, MemoryError> {
        match scope {
            RecordScope::Workspace(id) => {
                Ok(Scope::Workspace(self.stored_workspace_name(*id).await?))
            }
            RecordScope::Project(id) => {
                let (workspace, project) = self.stored_project_names(*id).await?;
                Ok(Scope::Project { workspace, project })
            }
            _ => self.scope_from_store(scope),
        }
    }

    async fn evidence_from_store(
        &self,
        evidence: &[RecordEvidence],
    ) -> Result<(Vec<Evidence>, Vec<Option<Name>>), MemoryError> {
        let mut output = Vec::with_capacity(evidence.len());
        let mut workspaces = Vec::with_capacity(evidence.len());
        let mut names: BTreeMap<ProjectId, (Name, Name)> = BTreeMap::new();
        for e in evidence {
            let (workspace, project) = match names.get(&e.project) {
                Some(names) => names.clone(),
                None => {
                    let resolved = self.stored_project_names(e.project).await?;
                    names.insert(e.project, resolved.clone());
                    resolved
                }
            };
            output.push(Evidence {
                project,
                view: ViewId::new(e.view.clone())
                    .map_err(|err| MemoryError::Invalid(format!("stored view: {err}")))?,
                commit: CommitId::new(e.commit.clone())
                    .map_err(|err| MemoryError::Invalid(format!("stored commit: {err}")))?,
                path: e.path.clone(),
                range: e.lines,
                content_hash: e.content_hash,
            });
            workspaces.push(Some(workspace));
        }
        Ok((output, workspaces))
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

    fn record_to_store(
        &self,
        record: &KnowledgeRecord,
        workspace: Option<&Name>,
    ) -> Result<NewRecord, MemoryError> {
        if let (Some(scope), Some(context)) = (Self::scope_workspace(&record.scope), workspace)
            && scope != context
        {
            return Err(MemoryError::Invalid(
                "memory scope does not match the evidence workspace".to_owned(),
            ));
        }
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
            evidence: self.evidence_to_store(
                &record.evidence,
                workspace.or_else(|| Self::scope_workspace(&record.scope)),
            )?,
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
        // Resolve canonical ids through hierarchy metadata even when a source
        // is not configured here. This neither registers nor opens that source.
        let (evidence, evidence_workspaces) = self.evidence_from_store(&stored.evidence).await?;
        let record = KnowledgeRecord {
            id: RecordId::from_uuid(stored.id.0),
            scope: self.stored_scope(&stored.scope).await?,
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
            evidence,
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
            evidence_workspaces,
        })
    }

    async fn rows_from_store(
        &self,
        stored: Vec<StoredRecord>,
    ) -> Result<Vec<RecordRow>, MemoryError> {
        let mut rows = Vec::with_capacity(stored.len());
        for record in stored {
            rows.push(self.record_from_store(record).await?);
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

impl StoreMemory {
    fn task_update(&self, before: &TaskRow, after: &Task) -> Result<TaskUpdate, MemoryError> {
        Ok(TaskUpdate {
            id: store_task_id(after.id),
            expected_revision: before.revision,
            title: after.title.clone(),
            goal: after.goal.clone(),
            status: status_to_store(after.status),
            details: self.task_to_details(after)?,
            updated_at: to_datetime(after.updated_at)?,
        })
    }
}

fn new_checkpoint(checkpoint: &Checkpoint) -> Result<NewCheckpoint, MemoryError> {
    Ok(NewCheckpoint {
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
    })
}

fn replayed(receipt: CheckpointReceipt) -> CheckpointSaved {
    CheckpointSaved::Replayed {
        task: TaskId::from_uuid(receipt.task.as_uuid()),
        seq: receipt.seq,
    }
}

impl MemoryRepo for StoreMemory {
    fn insert_record<'a>(
        &'a self,
        record: &'a KnowledgeRecord,
    ) -> BoxFuture<'a, Result<RecordRow, MemoryError>> {
        Box::pin(async move {
            let new = self.record_to_store(record, None)?;
            let mut conn = self.store.acquire().await.map_err(store_error)?;
            let stored = knowledge::insert_record(&mut conn, &new)
                .await
                .map_err(store_error)?;
            drop(conn);
            self.record_from_store(stored).await
        })
    }

    fn insert_record_in_workspace<'a>(
        &'a self,
        record: &'a KnowledgeRecord,
        workspace: &'a Name,
    ) -> BoxFuture<'a, Result<RecordRow, MemoryError>> {
        Box::pin(async move {
            let new = self.record_to_store(record, Some(workspace))?;
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
            if after.id != before.record.id || after.scope != before.record.scope {
                return Err(MemoryError::Invalid(
                    "memory identity and scope cannot change".to_owned(),
                ));
            }
            let current = {
                let mut conn = self.store.acquire().await.map_err(store_error)?;
                knowledge::get_record(&mut conn, record_id(before.record.id))
                    .await
                    .map_err(store_error)?
                    .filter(|record| record.organization == self.organization)
                    .ok_or_else(|| {
                        MemoryError::NotFound("memory record is unavailable".to_owned())
                    })?
            };
            if current.revision != before.revision || current.version != before.record.version {
                return Err(MemoryError::Conflict(
                    "memory changed since it was read".to_owned(),
                ));
            }
            // Release the connection before resolving hierarchy metadata, which
            // may acquire a connection of its own in a single-connection pool.
            let (evidence, workspaces) = self.evidence_from_store(&current.evidence).await?;
            if self.stored_scope(&current.scope).await? != before.record.scope
                || evidence != before.record.evidence
                || workspaces != before.evidence_workspaces
            {
                return Err(MemoryError::Conflict(
                    "saved memory identity changed since it was read".to_owned(),
                ));
            }
            if after.version == before.record.version && after.evidence != before.record.evidence {
                return Err(MemoryError::Invalid(
                    "changed memory evidence requires a new content version".to_owned(),
                ));
            }
            let appended = after
                .history
                .get(before.record.history.len()..)
                .unwrap_or_default();
            let content = if after.version != before.record.version {
                Some(RecordContent {
                    title: after.title.clone(),
                    body: after.body.clone(),
                    tags: after.tags.clone(),
                    evidence: self.with_directory(|directory| {
                        updated_evidence(
                            directory,
                            &before.record.evidence,
                            &after.evidence,
                            &current.evidence,
                            Self::scope_workspace(&after.scope),
                        )
                    })?,
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

    fn list_owned_tasks<'a>(
        &'a self,
        workspace: Option<&'a Name>,
        statuses: &'a [TaskStatus],
        owner: Option<&'a str>,
        limit: u32,
    ) -> BoxFuture<'a, Result<Vec<TaskRow>, MemoryError>> {
        Box::pin(async move {
            let workspace = match workspace {
                Some(name) => Some(self.with_directory(|d| d.workspace(name))?),
                None => None,
            };
            let wanted =
                usize::try_from(limit.clamp(1, tasks::MAX_TASKS_LISTED)).unwrap_or(usize::MAX);
            let mut filter = TaskFilter {
                organization: self.organization,
                workspace,
                owner: None,
                statuses: statuses.iter().copied().map(status_to_store).collect(),
                limit: tasks::MAX_TASKS_LISTED,
                before: None,
            };
            let mut conn = self.store.acquire().await.map_err(store_error)?;
            let mut rows = Vec::new();
            loop {
                let stored = tasks::list_tasks(&mut conn, &filter)
                    .await
                    .map_err(store_error)?;
                let exhausted = stored.len() < usize::try_from(filter.limit).unwrap_or(usize::MAX);
                // Keep database timestamp precision; domain timestamps cannot
                // safely locate the next page when several updates share a second.
                filter.before = stored.last().map(tasks::TaskCursor::after);
                for task in stored {
                    if task.owner.is_none() || task.owner.as_deref() == owner {
                        rows.push(self.task_from_store(task)?);
                        if rows.len() == wanted {
                            return Ok(rows);
                        }
                    }
                }
                if exhausted {
                    return Ok(rows);
                }
            }
        })
    }

    fn update_task<'a>(
        &'a self,
        before: &'a TaskRow,
        after: &'a Task,
    ) -> BoxFuture<'a, Result<TaskRow, MemoryError>> {
        Box::pin(async move {
            let update = self.task_update(before, after)?;
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
            let new = new_checkpoint(checkpoint)?;
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

    fn save_checkpoint<'a>(
        &'a self,
        save: CheckpointSave<'a>,
    ) -> BoxFuture<'a, Result<CheckpointSaved, MemoryError>> {
        Box::pin(async move {
            save.check()?;
            let receipt = save.receipt.map(CheckpointReceiptId);
            let decisions = save
                .decisions
                .iter()
                .map(|record| self.record_to_store(record, Some(save.workspace)))
                .collect::<Result<Vec<_>, _>>()?;
            let update = self.task_update(save.before, save.after)?;
            let checkpoint = new_checkpoint(save.checkpoint)?;
            let mut conn = self.store.acquire().await.map_err(store_error)?;
            let written = tasks::save_checkpoint(
                &mut conn,
                self.organization,
                receipt,
                &decisions,
                &update,
                &checkpoint,
            )
            .await
            .map_err(store_error)?;
            drop(conn);
            match written {
                CheckpointWrite::Replayed(found) => Ok(replayed(found)),
                CheckpointWrite::Saved(saved) => {
                    let saved = *saved;
                    Ok(CheckpointSaved::Saved {
                        task: Box::new(self.task_from_store(saved.task)?),
                        seq: saved.seq,
                        decisions: self.rows_from_store(saved.decisions).await?,
                    })
                }
            }
        })
    }

    fn checkpoint_receipt(
        &self,
        receipt: Uuid,
    ) -> BoxFuture<'_, Result<Option<(TaskId, u64)>, MemoryError>> {
        Box::pin(async move {
            let mut conn = self.store.acquire().await.map_err(store_error)?;
            let found = tasks::find_checkpoint_receipt(
                &mut conn,
                self.organization,
                CheckpointReceiptId(receipt),
            )
            .await
            .map_err(store_error)?;
            Ok(found.map(|r| (TaskId::from_uuid(r.task.as_uuid()), r.seq)))
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use knowell_core::{ContentHash, LineRange, RepoPath};
    use uuid::Uuid;

    fn pointer(path: &str) -> Evidence {
        Evidence {
            project: Name::new("synthetic-project").unwrap(),
            view: ViewId::new("branch:main").unwrap(),
            commit: CommitId::new("a".repeat(40)).unwrap(),
            path: RepoPath::new(path).unwrap(),
            range: LineRange::new(1, 1).unwrap(),
            content_hash: ContentHash::of(b"synthetic source"),
        }
    }

    fn namesakes() -> (Directory, Name, Name, ProjectId, ProjectId) {
        let mut directory = Directory::default();
        let private = Name::new("synthetic-private").unwrap();
        let public = Name::new("synthetic-public").unwrap();
        let private_id = ProjectId(Uuid::from_u128(11));
        let public_id = ProjectId(Uuid::from_u128(12));
        for (workspace, workspace_id, project_id) in
            [(&private, 1, private_id), (&public, 2, public_id)]
        {
            directory.add_workspace(
                workspace,
                WorkspaceId(Uuid::from_u128(workspace_id)),
                [(Name::new("synthetic-project").unwrap(), project_id)],
            );
        }
        (directory, private, public, private_id, public_id)
    }

    #[test]
    fn contextual_project_mapping_is_exact_and_legacy_ambiguity_remains_an_error() {
        let (directory, private, public, private_id, public_id) = namesakes();
        let project = Name::new("synthetic-project").unwrap();
        assert_eq!(
            directory
                .evidence_project(Some(&private), &project)
                .unwrap(),
            private_id
        );
        assert_eq!(
            directory.evidence_project(Some(&public), &project).unwrap(),
            public_id
        );
        assert!(matches!(
            directory.evidence_project(None, &project),
            Err(MemoryError::Invalid(_))
        ));
    }

    #[test]
    fn content_updates_preserve_canonical_ids_without_registered_origins() {
        let directory = Directory::default();
        let first = pointer("src/first.rs");
        let second = pointer("src/second.rs");
        let before = vec![first.clone(), second.clone()];
        let canonical = vec![
            stored_evidence(ProjectId(Uuid::from_u128(11)), &first),
            stored_evidence(ProjectId(Uuid::from_u128(12)), &second),
        ];
        assert_eq!(
            updated_evidence(&directory, &before, &before, &canonical, None).unwrap(),
            canonical
        );
        let after = vec![second, first];
        assert_eq!(
            updated_evidence(&directory, &before, &after, &canonical, None).unwrap(),
            canonical.iter().rev().cloned().collect::<Vec<_>>(),
        );
    }

    #[test]
    fn new_unscoped_evidence_does_not_guess_a_unique_registered_namesake() {
        let mut directory = Directory::default();
        let workspace = Name::new("synthetic-public").unwrap();
        let project_id = ProjectId(Uuid::from_u128(12));
        let evidence = pointer("src/KNOWELL_CANARY_POINTER.rs");
        directory.add_workspace(
            &workspace,
            WorkspaceId(Uuid::from_u128(2)),
            [(evidence.project.clone(), project_id)],
        );
        assert_eq!(
            directory.evidence_project(None, &evidence.project).unwrap(),
            project_id
        );
        let error = updated_evidence(&directory, &[], std::slice::from_ref(&evidence), &[], None)
            .unwrap_err();
        assert!(matches!(error, MemoryError::Invalid(_)));
        assert!(!error.to_string().contains("KNOWELL_CANARY"));
        assert_eq!(
            updated_evidence(
                &directory,
                &[],
                std::slice::from_ref(&evidence),
                &[],
                Some(&workspace)
            )
            .unwrap(),
            vec![stored_evidence(project_id, &evidence)],
        );
    }

    #[test]
    fn indistinguishable_saved_pointers_retain_order_but_reject_ambiguous_remaps() {
        let directory = Directory::default();
        let evidence = pointer("src/KNOWELL_CANARY_POINTER.rs");
        let before = vec![evidence.clone(), evidence.clone()];
        let canonical = vec![
            stored_evidence(ProjectId(Uuid::from_u128(11)), &evidence),
            stored_evidence(ProjectId(Uuid::from_u128(12)), &evidence),
        ];
        assert_eq!(
            updated_evidence(&directory, &before, &before, &canonical, None).unwrap(),
            canonical
        );
        let error = updated_evidence(
            &directory,
            &before,
            std::slice::from_ref(&evidence),
            &canonical,
            None,
        )
        .unwrap_err();
        assert!(matches!(error, MemoryError::Invalid(_)));
        assert!(!error.to_string().contains("KNOWELL_CANARY"));
    }
}
