//! The data model: scopes, kinds, states, actors, evidence and records.

use std::fmt;

use knowell_core::{ContentHash, LineRange, Name, RepoPath};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::ids::{
    ClientId, CommitId, RecordId, SessionId, Subject, SymbolId, TaskId, Timestamp, UserId, ViewId,
};

/// Where a record applies and who may see it.
#[derive(
    Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize, JsonSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum Scope {
    /// Shared across the whole organization.
    Organization,
    /// One workspace (a group of related projects).
    Workspace(Name),
    /// One project of a workspace.
    Project {
        /// Owning workspace.
        workspace: Name,
        /// The project.
        project: Name,
    },
    /// Private to one task.
    Task(TaskId),
    /// Private to one user.
    User(UserId),
}

impl Scope {
    /// Whether the two scopes can both apply to the same piece of work.
    ///
    /// An organization scope covers every workspace and project; a workspace
    /// covers its projects. Task and user scopes only overlap themselves
    /// because they are private and never inherited.
    pub fn overlaps(&self, other: &Scope) -> bool {
        match (self, other) {
            (Scope::Organization, Scope::Workspace(_) | Scope::Project { .. })
            | (Scope::Workspace(_) | Scope::Project { .. }, Scope::Organization) => true,
            (Scope::Workspace(w), Scope::Project { workspace, .. })
            | (Scope::Project { workspace, .. }, Scope::Workspace(w)) => w == workspace,
            (a, b) => a == b,
        }
    }

    /// Breadth for ordering: lower is broader (organization first).
    pub fn breadth(&self) -> u8 {
        match self {
            Scope::Organization => 0,
            Scope::Workspace(_) => 1,
            Scope::Project { .. } => 2,
            Scope::Task(_) => 3,
            Scope::User(_) => 4,
        }
    }

    /// A stable text key such as `project:shop/api`, used in rendered output.
    pub fn key(&self) -> String {
        match self {
            Scope::Organization => "org".to_string(),
            Scope::Workspace(w) => format!("workspace:{w}"),
            Scope::Project { workspace, project } => format!("project:{workspace}/{project}"),
            Scope::Task(t) => format!("task:{t}"),
            Scope::User(u) => format!("user:{u}"),
        }
    }
}

/// How a record came to exist.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize, JsonSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum RecordKind {
    /// Derived from code by deterministic analysis; carries evidence.
    Observed,
    /// Written by a human: an ADR, a decision, a rule.
    Human,
    /// A finding or remark by an agent. A draft until a human accepts it.
    ModelSuggestion,
}

impl fmt::Display for RecordKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            RecordKind::Observed => "observed",
            RecordKind::Human => "human",
            RecordKind::ModelSuggestion => "model suggestion",
        })
    }
}

/// Lifecycle state of a record. See the crate README for the state diagram.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize, JsonSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum RecordState {
    /// Waiting for review. Never presented as current.
    Proposed,
    /// Reviewed (or auto-accepted by policy); the only state presented as current.
    Accepted,
    /// Turned down or retracted. Terminal.
    Rejected,
    /// Its evidence no longer matches the code. Kept, flagged, awaiting revalidation.
    Stale,
    /// Replaced by a newer accepted record. Terminal.
    Superseded,
}

impl fmt::Display for RecordState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            RecordState::Proposed => "proposed",
            RecordState::Accepted => "accepted",
            RecordState::Rejected => "rejected",
            RecordState::Stale => "stale",
            RecordState::Superseded => "superseded",
        })
    }
}

/// Who did something.
#[derive(
    Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize, JsonSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum Actor {
    /// A human user.
    Human(UserId),
    /// An agent working in one session of one client.
    Agent {
        /// The session.
        session: SessionId,
        /// The client application.
        client: ClientId,
    },
    /// The engine itself (deterministic analysis, staleness checks).
    System,
}

impl fmt::Display for Actor {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Actor::Human(u) => write!(f, "human {u}"),
            Actor::Agent { session, client } => write!(f, "agent {client} (session {session})"),
            Actor::System => f.write_str("system"),
        }
    }
}

/// What was done to a record; recorded in its history and named in errors.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize, JsonSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum Action {
    /// Create a record in the proposed state.
    Propose,
    /// Accept a proposed or stale record.
    Accept,
    /// Reject or retract.
    Reject,
    /// Flag as stale.
    MarkStale,
    /// Confirm a stale record is still true.
    Revalidate,
    /// Replace by another record.
    Supersede,
    /// Create a new version with changed content.
    Edit,
    /// Pin or unpin.
    Pin,
}

impl fmt::Display for Action {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Action::Propose => "propose",
            Action::Accept => "accept",
            Action::Reject => "reject",
            Action::MarkStale => "mark stale",
            Action::Revalidate => "revalidate",
            Action::Supersede => "supersede",
            Action::Edit => "edit",
            Action::Pin => "pin",
        })
    }
}

/// A pointer to the code a record is based on.
///
/// The line range alone never identifies code; it is pinned to a view, a
/// commit and the content hash of the file version it was read from.
#[derive(
    Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize, JsonSchema,
)]
pub struct Evidence {
    /// The project the file belongs to.
    pub project: Name,
    /// The source view the evidence was taken from.
    pub view: ViewId,
    /// The commit of that view.
    pub commit: CommitId,
    /// File path relative to the project root.
    pub path: RepoPath,
    /// 1-based inclusive line span.
    pub range: LineRange,
    /// Hash of the file content the span was read from.
    pub content_hash: ContentHash,
}

/// One entry of a record's history log.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct HistoryEntry {
    /// When it happened.
    pub at: Timestamp,
    /// Who did it.
    pub actor: Actor,
    /// What was done.
    pub action: Action,
    /// State before; `None` for the creation entry.
    pub from: Option<RecordState>,
    /// State after.
    pub to: RecordState,
    /// Record version after the action.
    pub version: u32,
    /// Why (never contains secrets; checked on write).
    pub reason: String,
}

/// A superseded revision of a record's content, kept for audit.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct RecordVersion {
    /// The version number this content had.
    pub version: u32,
    /// Title at that version.
    pub title: String,
    /// Body at that version.
    pub body: String,
    /// Tags at that version.
    pub tags: Vec<String>,
    /// Evidence at that version.
    pub evidence: Vec<Evidence>,
    /// When that version stopped being current.
    pub replaced_at: Timestamp,
}

/// Input for creating a record.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct NewRecord {
    /// Identifier chosen by the caller.
    pub id: RecordId,
    /// Where the record applies.
    pub scope: Scope,
    /// How the record came to exist.
    pub kind: RecordKind,
    /// What the record is about.
    pub subject: Subject,
    /// Single-line title (at most 200 bytes).
    pub title: String,
    /// Markdown body (at most 32 KiB).
    pub body: String,
    /// Code the record is based on. Required for [`RecordKind::Observed`].
    pub evidence: Vec<Evidence>,
    /// Symbols the record is about.
    pub related_symbols: Vec<SymbolId>,
    /// Free tags; normalised to lowercase, sorted, deduplicated. The tag
    /// `rule` marks a record as a team rule for session bootstrap.
    pub tags: Vec<String>,
    /// Whether to pin the record into every bootstrap pack.
    pub pinned: bool,
}

/// A scoped, versioned, evidence-backed piece of knowledge.
///
/// Fields are public for reading and serialization. Mutate a record only
/// through its transition methods so state, version and history stay
/// consistent.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct KnowledgeRecord {
    /// Identifier.
    pub id: RecordId,
    /// Where the record applies.
    pub scope: Scope,
    /// How the record came to exist.
    pub kind: RecordKind,
    /// What the record is about.
    pub subject: Subject,
    /// Single-line title.
    pub title: String,
    /// Markdown body.
    pub body: String,
    /// Lifecycle state.
    pub state: RecordState,
    /// Content version, starting at 1; bumped by edits and evidence changes.
    pub version: u32,
    /// Who created it.
    pub author: Actor,
    /// Creation time.
    pub created_at: Timestamp,
    /// Time of the last change of any kind.
    pub updated_at: Timestamp,
    /// Code the record is based on.
    pub evidence: Vec<Evidence>,
    /// Symbols the record is about (sorted, deduplicated).
    pub related_symbols: Vec<SymbolId>,
    /// Normalised tags.
    pub tags: Vec<String>,
    /// Pinned into every bootstrap pack.
    pub pinned: bool,
    /// The record that replaced this one, if superseded.
    pub superseded_by: Option<RecordId>,
    /// Earlier content versions.
    pub previous_versions: Vec<RecordVersion>,
    /// Every state change, oldest first.
    pub history: Vec<HistoryEntry>,
}

impl KnowledgeRecord {
    /// Whether the record may be presented as current knowledge.
    /// Only accepted records are; stale, proposed, rejected and superseded
    /// records are kept but never shown as current.
    pub fn is_current(&self) -> bool {
        self.state == RecordState::Accepted
    }

    /// Whether the record carries the `rule` tag.
    pub fn is_rule(&self) -> bool {
        self.tags.iter().any(|t| t == RULE_TAG)
    }
}

/// The tag that marks a record as a team rule.
pub const RULE_TAG: &str = "rule";
