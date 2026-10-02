//! Tasks, checkpoints and the resume digest.

use std::collections::BTreeSet;
use std::fmt;

use knowell_core::{Name, RepoPath};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::error::KnowledgeError;
use crate::guard::check_no_secrets;
use crate::ids::{CommitId, RecordId, Subject, SymbolId, TaskId, Timestamp, ViewId};
use crate::model::{Action, Actor, KnowledgeRecord, RecordState, Scope};

const MAX_TEXT: usize = 8 * 1024;
const MAX_TITLE: usize = 200;

/// Lifecycle of a task.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize, JsonSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum TaskStatus {
    /// Created, not started.
    Open,
    /// Being worked on.
    InProgress,
    /// Waiting on something.
    Blocked,
    /// Finished. Terminal.
    Done,
    /// Dropped. Terminal.
    Abandoned,
}

impl TaskStatus {
    /// Whether the task can no longer change.
    pub fn is_terminal(self) -> bool {
        matches!(self, TaskStatus::Done | TaskStatus::Abandoned)
    }

    /// Whether `self -> to` is a legal change. `Open`, `InProgress` and
    /// `Blocked` move freely among themselves and into either terminal
    /// state; terminal states never move.
    pub fn can_become(self, to: TaskStatus) -> bool {
        self != to && !self.is_terminal()
    }
}

impl fmt::Display for TaskStatus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            TaskStatus::Open => "open",
            TaskStatus::InProgress => "in progress",
            TaskStatus::Blocked => "blocked",
            TaskStatus::Done => "done",
            TaskStatus::Abandoned => "abandoned",
        })
    }
}

/// A free-text progress note.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct ProgressNote {
    /// When it was written.
    pub at: Timestamp,
    /// Who wrote it.
    pub author: Actor,
    /// The note (secret-checked on write).
    pub text: String,
}

/// A question the task has not answered yet.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct OpenQuestion {
    /// Number, unique within the task, assigned in order starting at 1.
    pub id: u32,
    /// The question.
    pub text: String,
    /// When it was asked.
    pub asked_at: Timestamp,
    /// When it was resolved; `None` while open.
    pub resolved_at: Option<Timestamp>,
}

/// A file a task is about.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize, JsonSchema)]
pub struct FileRef {
    /// Project of the file.
    pub project: Name,
    /// Path relative to the project root.
    pub path: RepoPath,
}

/// The exact source state of one project view a task worked against.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize, JsonSchema)]
pub struct ManifestPin {
    /// The project.
    pub project: Name,
    /// The view (ref, worktree, snapshot).
    pub view: ViewId,
    /// The commit the view was at.
    pub commit: CommitId,
    /// Generation counter of the local (uncommitted) layer, if any.
    pub local_generation: Option<u64>,
}

/// A unit of work an agent (or a human) can leave and resume, possibly on
/// another machine.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct Task {
    /// Identifier.
    pub id: TaskId,
    /// Single-line title.
    pub title: String,
    /// What done looks like.
    pub goal: String,
    /// Lifecycle status.
    pub status: TaskStatus,
    /// Creation time.
    pub created_at: Timestamp,
    /// Last change.
    pub updated_at: Timestamp,
    /// Progress notes, oldest first.
    pub notes: Vec<ProgressNote>,
    /// Decisions taken during the task, as record ids.
    pub decisions: Vec<RecordId>,
    /// Questions, open and resolved.
    pub open_questions: Vec<OpenQuestion>,
    /// Symbols the task touches (sorted, deduplicated).
    pub related_symbols: Vec<SymbolId>,
    /// Files the task touches (sorted, deduplicated).
    pub related_files: Vec<FileRef>,
    /// Source state the task is working against (sorted by project, view).
    pub view_manifest: Vec<ManifestPin>,
}

fn clean_text(field: &'static str, text: &str, limit: usize) -> Result<String, KnowledgeError> {
    let text = text.trim();
    if text.is_empty() {
        return Err(KnowledgeError::EmptyField(field));
    }
    if text.len() > limit {
        return Err(KnowledgeError::TooLong { field, limit });
    }
    check_no_secrets(field, text)?;
    Ok(text.to_string())
}

fn sorted_manifest(mut manifest: Vec<ManifestPin>) -> Vec<ManifestPin> {
    manifest.sort();
    manifest.dedup();
    manifest
}

impl Task {
    /// Creates an open task.
    pub fn new(id: TaskId, title: &str, goal: &str, at: Timestamp) -> Result<Self, KnowledgeError> {
        let title = clean_text("title", title, MAX_TITLE)?;
        if title.contains(['\n', '\r']) {
            return Err(KnowledgeError::InvalidField {
                field: "title",
                reason: "must be a single line",
            });
        }
        Ok(Self {
            id,
            title,
            goal: clean_text("goal", goal, MAX_TEXT)?,
            status: TaskStatus::Open,
            created_at: at,
            updated_at: at,
            notes: Vec::new(),
            decisions: Vec::new(),
            open_questions: Vec::new(),
            related_symbols: Vec::new(),
            related_files: Vec::new(),
            view_manifest: Vec::new(),
        })
    }

    fn ensure_open(&self) -> Result<(), KnowledgeError> {
        if self.status.is_terminal() {
            Err(KnowledgeError::TaskClosed(self.status))
        } else {
            Ok(())
        }
    }

    /// Changes the status. Terminal tasks cannot change; unchanged status is an error.
    pub fn set_status(&mut self, to: TaskStatus, at: Timestamp) -> Result<(), KnowledgeError> {
        self.ensure_open()?;
        if !self.status.can_become(to) {
            return Err(KnowledgeError::IllegalTaskTransition {
                from: self.status,
                to,
            });
        }
        self.status = to;
        self.updated_at = at;
        Ok(())
    }

    /// Appends a progress note.
    pub fn add_note(
        &mut self,
        author: Actor,
        text: &str,
        at: Timestamp,
    ) -> Result<(), KnowledgeError> {
        self.ensure_open()?;
        let text = clean_text("note", text, MAX_TEXT)?;
        self.notes.push(ProgressNote { at, author, text });
        self.updated_at = at;
        Ok(())
    }

    /// Records a decision (a knowledge record id) taken during the task.
    /// Adding the same record twice is a no-op.
    pub fn add_decision(&mut self, record: RecordId, at: Timestamp) -> Result<(), KnowledgeError> {
        self.ensure_open()?;
        if !self.decisions.contains(&record) {
            self.decisions.push(record);
            self.updated_at = at;
        }
        Ok(())
    }

    /// Adds an open question and returns its number.
    pub fn ask_question(&mut self, text: &str, at: Timestamp) -> Result<u32, KnowledgeError> {
        self.ensure_open()?;
        let text = clean_text("question", text, MAX_TEXT)?;
        let id = self
            .open_questions
            .iter()
            .map(|q| q.id)
            .max()
            .map_or(1, |m| m.saturating_add(1));
        self.open_questions.push(OpenQuestion {
            id,
            text,
            asked_at: at,
            resolved_at: None,
        });
        self.updated_at = at;
        Ok(id)
    }

    /// Marks a question resolved. Resolving twice is an error.
    pub fn resolve_question(&mut self, id: u32, at: Timestamp) -> Result<(), KnowledgeError> {
        self.ensure_open()?;
        match self
            .open_questions
            .iter_mut()
            .find(|q| q.id == id && q.resolved_at.is_none())
        {
            Some(q) => {
                q.resolved_at = Some(at);
                self.updated_at = at;
                Ok(())
            }
            None => Err(KnowledgeError::UnknownQuestion(id)),
        }
    }

    /// Links symbols and files the task is about.
    pub fn link(
        &mut self,
        symbols: Vec<SymbolId>,
        files: Vec<FileRef>,
        at: Timestamp,
    ) -> Result<(), KnowledgeError> {
        self.ensure_open()?;
        let syms: BTreeSet<SymbolId> = self.related_symbols.drain(..).chain(symbols).collect();
        let files: BTreeSet<FileRef> = self.related_files.drain(..).chain(files).collect();
        self.related_symbols = syms.into_iter().collect();
        self.related_files = files.into_iter().collect();
        self.updated_at = at;
        Ok(())
    }

    /// Replaces the view manifest with the source state now being worked on.
    pub fn set_manifest(
        &mut self,
        manifest: Vec<ManifestPin>,
        at: Timestamp,
    ) -> Result<(), KnowledgeError> {
        self.ensure_open()?;
        self.view_manifest = sorted_manifest(manifest);
        self.updated_at = at;
        Ok(())
    }

    /// Snapshots the task into a [`Checkpoint`]. Does not change the task.
    pub fn checkpoint(
        &self,
        summary: &str,
        next_steps: Vec<String>,
        at: Timestamp,
    ) -> Result<Checkpoint, KnowledgeError> {
        let summary = clean_text("summary", summary, MAX_TEXT)?;
        let next_steps = next_steps
            .iter()
            .map(|s| clean_text("next step", s, MAX_TEXT))
            .collect::<Result<Vec<_>, _>>()?;
        Ok(Checkpoint {
            task: self.id,
            at,
            summary,
            decisions: self.decisions.clone(),
            next_steps,
            manifest: self.view_manifest.clone(),
        })
    }
}

/// A saved point of progress to resume from.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct Checkpoint {
    /// The task.
    pub task: TaskId,
    /// When it was saved.
    pub at: Timestamp,
    /// What has been done and learned.
    pub summary: String,
    /// Decisions taken so far.
    pub decisions: Vec<RecordId>,
    /// What to do next.
    pub next_steps: Vec<String>,
    /// The source state at the time.
    pub manifest: Vec<ManifestPin>,
}

/// What the digest was compared against.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum Baseline {
    /// The latest checkpoint, saved at this time.
    Checkpoint(Timestamp),
    /// No checkpoint exists; the task's own manifest was used.
    TaskManifest,
}

/// How a pinned view differs from the baseline.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ManifestChangeKind {
    /// The commit or the local generation moved.
    Moved {
        /// Commit at the baseline.
        from_commit: CommitId,
        /// Commit now.
        to_commit: CommitId,
        /// Local generation at the baseline.
        from_generation: Option<u64>,
        /// Local generation now.
        to_generation: Option<u64>,
    },
    /// The view is present now but was not at the baseline.
    Added {
        /// Commit now.
        commit: CommitId,
    },
    /// The view was at the baseline but is gone now.
    Removed {
        /// Commit at the baseline.
        commit: CommitId,
    },
}

/// One changed view.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct ManifestChange {
    /// The project.
    pub project: Name,
    /// The view.
    pub view: ViewId,
    /// What changed.
    pub kind: ManifestChangeKind,
}

/// Why a recorded decision needs attention.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum DecisionProblem {
    /// Its evidence changed. `since_checkpoint` tells whether that happened
    /// after the baseline checkpoint.
    Stale {
        /// Whether it went stale after the checkpoint.
        since_checkpoint: bool,
    },
    /// It was replaced.
    Superseded {
        /// The replacement, if known.
        by: Option<RecordId>,
    },
    /// It was rejected.
    Rejected,
    /// It was never accepted.
    Unreviewed,
    /// The record was not found in the supplied records.
    Missing,
}

/// A decision of the task that is no longer a current, accepted record.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct DecisionIssue {
    /// The record.
    pub record: RecordId,
    /// Its subject, if the record was found.
    pub subject: Option<Subject>,
    /// Its title, if the record was found.
    pub title: Option<String>,
    /// What is wrong.
    pub problem: DecisionProblem,
}

/// A reference to a stale record in a moved project.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct StaleRecordRef {
    /// The record.
    pub record: RecordId,
    /// Its subject.
    pub subject: Subject,
    /// Its title.
    pub title: String,
}

/// Everything an agent needs to know to continue a task after a break.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct ResumeDigest {
    /// The task.
    pub task: TaskId,
    /// Its title.
    pub title: String,
    /// Its goal.
    pub goal: String,
    /// Its status.
    pub status: TaskStatus,
    /// What `manifest_changes` was computed against.
    pub baseline: Baseline,
    /// Summary of the last checkpoint, if any.
    pub last_summary: Option<String>,
    /// Next steps of the last checkpoint.
    pub next_steps: Vec<String>,
    /// Views whose pinned commit or local generation moved, appeared or vanished.
    /// Sorted by project, view.
    pub manifest_changes: Vec<ManifestChange>,
    /// The task's decisions that are no longer current. Sorted by record id.
    pub decision_issues: Vec<DecisionIssue>,
    /// Other stale records scoped to a project whose view changed. Sorted by record id.
    pub stale_in_changed_projects: Vec<StaleRecordRef>,
    /// Questions still open.
    pub open_questions: Vec<OpenQuestion>,
}

impl ResumeDigest {
    /// Whether nothing changed since the baseline and nothing went stale.
    pub fn is_unchanged(&self) -> bool {
        self.manifest_changes.is_empty()
            && self.decision_issues.is_empty()
            && self.stale_in_changed_projects.is_empty()
    }
}

fn diff_manifest(baseline: &[ManifestPin], current: &[ManifestPin]) -> Vec<ManifestChange> {
    use std::collections::BTreeMap;
    let key = |p: &ManifestPin| (p.project.clone(), p.view.clone());
    let old: BTreeMap<_, _> = baseline.iter().map(|p| (key(p), p)).collect();
    let new: BTreeMap<_, _> = current.iter().map(|p| (key(p), p)).collect();
    let keys: BTreeSet<_> = old.keys().chain(new.keys()).cloned().collect();
    let mut out = Vec::new();
    for k in keys {
        let kind = match (old.get(&k), new.get(&k)) {
            (Some(a), Some(b))
                if a.commit != b.commit || a.local_generation != b.local_generation =>
            {
                Some(ManifestChangeKind::Moved {
                    from_commit: a.commit.clone(),
                    to_commit: b.commit.clone(),
                    from_generation: a.local_generation,
                    to_generation: b.local_generation,
                })
            }
            (None, Some(b)) => Some(ManifestChangeKind::Added {
                commit: b.commit.clone(),
            }),
            (Some(a), None) => Some(ManifestChangeKind::Removed {
                commit: a.commit.clone(),
            }),
            _ => None,
        };
        if let Some(kind) = kind {
            out.push(ManifestChange {
                project: k.0,
                view: k.1,
                kind,
            });
        }
    }
    out
}

/// Builds the digest for resuming `task`.
///
/// `checkpoints` may contain checkpoints of other tasks; only this task's are
/// used, and the latest one (by time; later in the slice wins ties) is the
/// baseline. Without a checkpoint the task's own manifest is the baseline.
/// `current_manifest` is the source state now. `records` are the knowledge
/// records visible to the task.
///
/// The digest lists views whose pinned commit moved, task decisions that are
/// no longer accepted (stale, superseded, rejected, unreviewed or missing)
/// and further stale records scoped to projects whose view changed. It
/// never hides a stale record: stale knowledge is reported as stale.
pub fn resume(
    task: &Task,
    checkpoints: &[Checkpoint],
    current_manifest: &[ManifestPin],
    records: &[KnowledgeRecord],
) -> ResumeDigest {
    let last =
        checkpoints
            .iter()
            .filter(|c| c.task == task.id)
            .fold(None::<&Checkpoint>, |best, c| match best {
                Some(b) if b.at > c.at => Some(b),
                _ => Some(c),
            });
    let (baseline, baseline_manifest): (Baseline, &[ManifestPin]) = match last {
        Some(c) => (Baseline::Checkpoint(c.at), &c.manifest),
        None => (Baseline::TaskManifest, &task.view_manifest),
    };
    let manifest_changes = diff_manifest(baseline_manifest, current_manifest);

    let decision_ids: BTreeSet<RecordId> = task
        .decisions
        .iter()
        .chain(last.iter().flat_map(|c| c.decisions.iter()))
        .copied()
        .collect();
    let mut decision_issues = Vec::new();
    for id in &decision_ids {
        let Some(record) = records.iter().find(|r| r.id == *id) else {
            decision_issues.push(DecisionIssue {
                record: *id,
                subject: None,
                title: None,
                problem: DecisionProblem::Missing,
            });
            continue;
        };
        let problem = match record.state {
            RecordState::Accepted => continue,
            RecordState::Stale => {
                let since_checkpoint = match last {
                    None => true,
                    Some(c) => record
                        .history
                        .iter()
                        .rev()
                        .find(|h| h.action == Action::MarkStale)
                        .is_none_or(|h| h.at > c.at),
                };
                DecisionProblem::Stale { since_checkpoint }
            }
            RecordState::Superseded => DecisionProblem::Superseded {
                by: record.superseded_by,
            },
            RecordState::Rejected => DecisionProblem::Rejected,
            RecordState::Proposed => DecisionProblem::Unreviewed,
        };
        decision_issues.push(DecisionIssue {
            record: record.id,
            subject: Some(record.subject.clone()),
            title: Some(record.title.clone()),
            problem,
        });
    }

    let changed_projects: BTreeSet<&Name> = manifest_changes.iter().map(|c| &c.project).collect();
    let mut stale_in_changed_projects: Vec<StaleRecordRef> = records
        .iter()
        .filter(|r| r.state == RecordState::Stale && !decision_ids.contains(&r.id))
        .filter(|r| matches!(&r.scope, Scope::Project { project, .. } if changed_projects.contains(project)))
        .map(|r| StaleRecordRef { record: r.id, subject: r.subject.clone(), title: r.title.clone() })
        .collect();
    stale_in_changed_projects.sort_by_key(|r| r.record);

    ResumeDigest {
        task: task.id,
        title: task.title.clone(),
        goal: task.goal.clone(),
        status: task.status,
        baseline,
        last_summary: last.map(|c| c.summary.clone()),
        next_steps: last.map(|c| c.next_steps.clone()).unwrap_or_default(),
        manifest_changes,
        decision_issues,
        stale_in_changed_projects,
        open_questions: task
            .open_questions
            .iter()
            .filter(|q| q.resolved_at.is_none())
            .cloned()
            .collect(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::RecordKind;
    use crate::policy::{AcceptancePolicy, Rights};
    use crate::testutil::*;
    use uuid::Uuid;

    fn task() -> Task {
        Task::new(
            TaskId::from_uuid(Uuid::from_u128(1)),
            "Add idempotency",
            "Charges are idempotent",
            ts(1),
        )
        .unwrap()
    }

    fn pin(project: &str, commit: &str, generation: Option<u64>) -> ManifestPin {
        ManifestPin {
            project: name(project),
            view: ViewId::new("main").unwrap(),
            commit: CommitId::new(commit).unwrap(),
            local_generation: generation,
        }
    }

    fn accepted_record(n: u128, scope: Scope) -> KnowledgeRecord {
        let mut new = new_record(RecordKind::Human, n);
        new.scope = scope;
        KnowledgeRecord::write(
            new,
            human("a"),
            Rights::REVIEWER,
            &AcceptancePolicy::default(),
            ts(1),
        )
        .unwrap()
        .record
    }

    #[test]
    fn task_creation_validates() {
        assert!(Task::new(TaskId::generate(), " ", "g", ts(1)).is_err());
        assert!(Task::new(TaskId::generate(), "t", "", ts(1)).is_err());
        assert!(Task::new(TaskId::generate(), "a\nb", "g", ts(1)).is_err());
        let t = task();
        assert_eq!(t.status, TaskStatus::Open);
    }

    #[test]
    fn status_transitions() {
        let mut t = task();
        t.set_status(TaskStatus::InProgress, ts(2)).unwrap();
        t.set_status(TaskStatus::Blocked, ts(3)).unwrap();
        t.set_status(TaskStatus::Open, ts(4)).unwrap();
        assert_eq!(
            t.set_status(TaskStatus::Open, ts(5)),
            Err(KnowledgeError::IllegalTaskTransition {
                from: TaskStatus::Open,
                to: TaskStatus::Open
            })
        );
        t.set_status(TaskStatus::Done, ts(6)).unwrap();
        assert_eq!(
            t.set_status(TaskStatus::Open, ts(7)),
            Err(KnowledgeError::TaskClosed(TaskStatus::Done))
        );
        assert_eq!(
            t.add_note(human("a"), "x", ts(8)),
            Err(KnowledgeError::TaskClosed(TaskStatus::Done))
        );
        let mut t = task();
        t.set_status(TaskStatus::Abandoned, ts(2)).unwrap();
        assert!(t.set_status(TaskStatus::InProgress, ts(3)).is_err());
    }

    #[test]
    fn notes_questions_decisions_links() {
        let mut t = task();
        t.add_note(human("a"), "  started  ", ts(2)).unwrap();
        assert_eq!(t.notes[0].text, "started");
        let q1 = t.ask_question("which header?", ts(3)).unwrap();
        let q2 = t.ask_question("retry policy?", ts(4)).unwrap();
        assert_eq!((q1, q2), (1, 2));
        t.resolve_question(1, ts(5)).unwrap();
        assert_eq!(
            t.resolve_question(1, ts(6)),
            Err(KnowledgeError::UnknownQuestion(1))
        );
        assert_eq!(
            t.resolve_question(9, ts(6)),
            Err(KnowledgeError::UnknownQuestion(9))
        );
        t.add_decision(rid(5), ts(7)).unwrap();
        t.add_decision(rid(5), ts(8)).unwrap();
        assert_eq!(t.decisions, vec![rid(5)]);
        let file = FileRef {
            project: name("api"),
            path: RepoPath::new("src/a.rs").unwrap(),
        };
        t.link(vec![sym("b"), sym("a")], vec![file.clone()], ts(9))
            .unwrap();
        t.link(vec![sym("a")], vec![file.clone()], ts(10)).unwrap();
        assert_eq!(t.related_symbols, vec![sym("a"), sym("b")]);
        assert_eq!(t.related_files, vec![file]);
    }

    #[test]
    fn task_text_is_secret_checked() {
        let mut t = task();
        let leaked = format!("token {}", fake_token());
        assert!(matches!(
            t.add_note(human("a"), &leaked, ts(2)),
            Err(KnowledgeError::SecretDetected { field: "note", .. })
        ));
        assert!(t.ask_question(&leaked, ts(2)).is_err());
        assert!(t.checkpoint(&leaked, vec![], ts(2)).is_err());
        assert!(t.checkpoint("ok", vec![leaked], ts(2)).is_err());
        assert!(t.notes.is_empty());
    }

    #[test]
    fn checkpoint_snapshots_decisions_and_manifest() {
        let mut t = task();
        t.add_decision(rid(5), ts(2)).unwrap();
        t.set_manifest(
            vec![pin("web", "bbbbbbb", None), pin("api", "aaaaaaa", Some(3))],
            ts(3),
        )
        .unwrap();
        let cp = t
            .checkpoint("did the thing", vec!["write tests".into()], ts(4))
            .unwrap();
        assert_eq!(cp.task, t.id);
        assert_eq!(cp.decisions, vec![rid(5)]);
        assert_eq!(cp.manifest[0].project.as_str(), "api", "manifest is sorted");
        assert_eq!(cp.next_steps, vec!["write tests"]);
    }

    #[test]
    fn resume_reports_moved_added_removed_views() {
        let mut t = task();
        t.set_manifest(
            vec![
                pin("api", "aaaaaaa", None),
                pin("web", "bbbbbbb", None),
                pin("old", "ccccccc", None),
            ],
            ts(2),
        )
        .unwrap();
        let cp = t.checkpoint("s", vec!["n".into()], ts(3)).unwrap();
        let current = vec![
            pin("api", "aaaaaaa", None),
            pin("web", "ddddddd", None),
            pin("new", "eeeeeee", None),
        ];
        let digest = resume(&t, &[cp], &current, &[]);
        assert_eq!(digest.baseline, Baseline::Checkpoint(ts(3)));
        let summary: Vec<(String, &str)> = digest
            .manifest_changes
            .iter()
            .map(|c| {
                let k = match c.kind {
                    ManifestChangeKind::Moved { .. } => "moved",
                    ManifestChangeKind::Added { .. } => "added",
                    ManifestChangeKind::Removed { .. } => "removed",
                };
                (c.project.to_string(), k)
            })
            .collect();
        assert_eq!(
            summary,
            vec![
                ("new".into(), "added"),
                ("old".into(), "removed"),
                ("web".into(), "moved")
            ]
        );
        assert_eq!(digest.last_summary.as_deref(), Some("s"));
        assert_eq!(digest.next_steps, vec!["n"]);
        assert!(!digest.is_unchanged());
    }

    #[test]
    fn resume_detects_local_generation_change() {
        let mut t = task();
        t.set_manifest(vec![pin("api", "aaaaaaa", Some(1))], ts(2))
            .unwrap();
        let digest = resume(&t, &[], &[pin("api", "aaaaaaa", Some(2))], &[]);
        assert_eq!(digest.baseline, Baseline::TaskManifest);
        assert!(matches!(
            digest.manifest_changes[0].kind,
            ManifestChangeKind::Moved {
                from_generation: Some(1),
                to_generation: Some(2),
                ..
            }
        ));
    }

    #[test]
    fn resume_unchanged() {
        let mut t = task();
        t.set_manifest(vec![pin("api", "aaaaaaa", None)], ts(2))
            .unwrap();
        let cp = t.checkpoint("s", vec![], ts(3)).unwrap();
        let digest = resume(&t, &[cp], &[pin("api", "aaaaaaa", None)], &[]);
        assert!(digest.is_unchanged());
    }

    #[test]
    fn resume_uses_latest_checkpoint_of_this_task_only() {
        let mut t = task();
        t.set_manifest(vec![pin("api", "aaaaaaa", None)], ts(2))
            .unwrap();
        let old = t.checkpoint("old", vec![], ts(3)).unwrap();
        t.set_manifest(vec![pin("api", "bbbbbbb", None)], ts(4))
            .unwrap();
        let newer = t.checkpoint("newer", vec![], ts(5)).unwrap();
        let mut foreign = newer.clone();
        foreign.task = TaskId::from_uuid(Uuid::from_u128(99));
        foreign.at = ts(100);
        foreign.summary = "foreign".into();
        let digest = resume(
            &t,
            &[newer, foreign, old],
            &[pin("api", "bbbbbbb", None)],
            &[],
        );
        assert_eq!(digest.last_summary.as_deref(), Some("newer"));
        assert!(digest.manifest_changes.is_empty());
    }

    #[test]
    fn resume_lists_decision_problems() {
        let mut t = task();
        let fine = accepted_record(1, project_scope("api"));
        let mut stale_after = accepted_record(2, project_scope("api"));
        stale_after
            .mark_stale(&Actor::System, "changed", ts(10))
            .unwrap();
        let mut stale_before = accepted_record(3, project_scope("api"));
        stale_before
            .mark_stale(&Actor::System, "changed", ts(2))
            .unwrap();
        let mut gone = accepted_record(4, project_scope("api"));
        let by = accepted_record(40, project_scope("api"));
        gone.supersede(&by, &human("r"), Rights::REVIEWER, "newer", ts(3))
            .unwrap();
        let mut rej = accepted_record(5, project_scope("api"));
        rej.reject(&human("r"), Rights::REVIEWER, "no", ts(3))
            .unwrap();
        let unreviewed =
            KnowledgeRecord::propose(new_record(RecordKind::Human, 6), human("a"), ts(1)).unwrap();
        for n in 1..=7u128 {
            t.add_decision(rid(n), ts(2)).unwrap();
        }
        let cp = t.checkpoint("s", vec![], ts(5)).unwrap();
        let digest = resume(
            &t,
            &[cp],
            &[],
            &[fine, stale_after, stale_before, gone, rej, unreviewed],
        );
        let problems: Vec<_> = digest
            .decision_issues
            .iter()
            .map(|i| (i.record, i.problem.clone()))
            .collect();
        assert_eq!(
            problems,
            vec![
                (
                    rid(2),
                    DecisionProblem::Stale {
                        since_checkpoint: true
                    }
                ),
                (
                    rid(3),
                    DecisionProblem::Stale {
                        since_checkpoint: false
                    }
                ),
                (rid(4), DecisionProblem::Superseded { by: Some(rid(40)) }),
                (rid(5), DecisionProblem::Rejected),
                (rid(6), DecisionProblem::Unreviewed),
                (rid(7), DecisionProblem::Missing),
            ]
        );
    }

    #[test]
    fn resume_lists_stale_records_of_changed_projects() {
        let mut t = task();
        t.set_manifest(
            vec![pin("api", "aaaaaaa", None), pin("web", "bbbbbbb", None)],
            ts(2),
        )
        .unwrap();
        let mut in_api = accepted_record(1, project_scope("api"));
        in_api.mark_stale(&Actor::System, "x", ts(3)).unwrap();
        let mut in_web = accepted_record(2, project_scope("web"));
        in_web.mark_stale(&Actor::System, "x", ts(3)).unwrap();
        let cp = t.checkpoint("s", vec![], ts(4)).unwrap();
        let digest = resume(
            &t,
            &[cp],
            &[pin("api", "ccccccc", None), pin("web", "bbbbbbb", None)],
            &[in_api, in_web],
        );
        assert_eq!(digest.stale_in_changed_projects.len(), 1);
        assert_eq!(digest.stale_in_changed_projects[0].record, rid(1));
    }

    #[test]
    fn resume_lists_only_unresolved_questions() {
        let mut t = task();
        t.ask_question("a?", ts(2)).unwrap();
        t.ask_question("b?", ts(2)).unwrap();
        t.resolve_question(1, ts(3)).unwrap();
        let digest = resume(&t, &[], &[], &[]);
        assert_eq!(digest.open_questions.len(), 1);
        assert_eq!(digest.open_questions[0].text, "b?");
    }

    #[test]
    fn resume_is_deterministic_regardless_of_input_order() {
        let mut t = task();
        t.set_manifest(
            vec![pin("api", "aaaaaaa", None), pin("web", "bbbbbbb", None)],
            ts(2),
        )
        .unwrap();
        let cp = t.checkpoint("s", vec![], ts(3)).unwrap();
        let a = [pin("api", "ccccccc", None), pin("web", "ddddddd", None)];
        let b = [pin("web", "ddddddd", None), pin("api", "ccccccc", None)];
        assert_eq!(
            resume(&t, std::slice::from_ref(&cp), &a, &[]),
            resume(&t, &[cp], &b, &[])
        );
    }
}
