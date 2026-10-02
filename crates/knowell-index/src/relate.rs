//! T3: the relation hook (cross-project contract linking with
//! [`crate::LinkRelationStage`] by default) and the knowledge-staleness
//! signal. T3 runs before activation, so relations are part of the
//! generation that becomes searchable.

use std::sync::Arc;

use knowell_core::{ContentHash, Name, RepoPath};
use knowell_parse::ParsedFile;
use knowell_store::graph::{NewContract, NewEdge};
use knowell_store::{ProjectId, ViewId, WorkspaceId};
use serde::Serialize;

/// One changed file handed to a [`RelationStage`].
#[derive(Debug, Clone)]
pub struct RelationFile {
    /// Project-relative path.
    pub path: RepoPath,
    /// Content hash of the file version.
    pub content_hash: ContentHash,
    /// Redacted text (the only text the engine ever stores or hands out).
    pub text: Arc<str>,
    /// Analysis of that text.
    pub parsed: Arc<ParsedFile>,
}

/// What a [`RelationStage`] sees of one build.
#[derive(Debug)]
pub struct RelationInput<'a> {
    /// Workspace of the project.
    pub workspace: WorkspaceId,
    /// Project being built.
    pub project: ProjectId,
    /// Its name.
    pub project_name: &'a Name,
    /// The view.
    pub view: ViewId,
    /// The generation being built.
    pub generation: i64,
    /// Files added, modified or renamed in this generation, by path.
    pub changed: &'a [RelationFile],
    /// Paths removed in this generation (including old paths of renames).
    pub removed: &'a [RepoPath],
}

/// What a [`RelationStage`] wants written for the generation.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct RelationOutput {
    /// Origins whose relation rows are replaced (by convention
    /// `<stage name>:<path>`). Every edge and contract must carry one of
    /// them; listing an origin with no rows removes its earlier rows.
    pub origins: Vec<String>,
    /// Edges to record (contract-derived, heuristic, ...).
    pub edges: Vec<NewEdge>,
    /// Contract participations to record.
    pub contracts: Vec<NewContract>,
}

/// Error of a [`RelationStage`]; the message is shown as the T3 failure.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{0}")]
pub struct RelationError(pub String);

/// The T3 hook: derives relations (contract links, cross-project edges)
/// from the files a build changed. Runs on a blocking thread; must be
/// deterministic and must not panic.
///
/// Origins must start with `<name>:` so relation rows never collide with
/// the syntactic edges T1 writes (whose origin is the plain path).
pub trait RelationStage: Send + Sync {
    /// Stable name, used as the origin prefix.
    fn name(&self) -> &str;

    /// Relations for one build.
    fn relate(&self, input: &RelationInput<'_>) -> Result<RelationOutput, RelationError>;
}

/// A relation stage that writes nothing. The default stage of an indexer is
/// [`crate::LinkRelationStage`] (contract linking); pass this one to
/// [`crate::IndexerBuilder::relation_stage`] to record no relations.
#[derive(Debug, Clone, Copy, Default)]
pub struct NoRelations;

impl RelationStage for NoRelations {
    fn name(&self) -> &str {
        "none"
    }

    fn relate(&self, _input: &RelationInput<'_>) -> Result<RelationOutput, RelationError> {
        Ok(RelationOutput::default())
    }
}

/// A file whose previous version changed or disappeared when a generation
/// became active: knowledge with evidence in it may be stale.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize)]
pub struct StaleFile {
    /// Project-relative path in the previous generation.
    pub path: RepoPath,
    /// Content hash knowledge evidence may still point to.
    pub old_hash: ContentHash,
    /// The new content, `None` when the file was deleted or moved away.
    pub new_hash: Option<ContentHash>,
    /// New path when the file was renamed.
    pub renamed_to: Option<RepoPath>,
}

/// Emitted once per activated generation that changed or removed files.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct StalenessEvent {
    /// The project.
    pub project: ProjectId,
    /// Its name.
    pub project_name: Name,
    /// The view.
    pub view: ViewId,
    /// The generation that became active.
    pub generation: i64,
    /// The generation it replaced.
    pub previous_generation: Option<i64>,
    /// Changed or removed files, sorted by path.
    pub files: Vec<StaleFile>,
}

/// Checks that every origin of `output` carries the stage's prefix and that
/// every row belongs to a listed origin.
pub(crate) fn validate_output(stage: &str, output: &RelationOutput) -> Result<(), RelationError> {
    let prefix = format!("{stage}:");
    if let Some(bad) = output.origins.iter().find(|o| !o.starts_with(&prefix)) {
        return Err(RelationError(format!(
            "relation stage `{stage}` used origin `{bad}`; origins must start with `{prefix}`"
        )));
    }
    let listed = |origin: &str| output.origins.iter().any(|o| o == origin);
    if let Some(edge) = output.edges.iter().find(|e| !listed(&e.origin)) {
        return Err(RelationError(format!(
            "relation stage `{stage}` wrote an edge with unlisted origin `{}`",
            edge.origin
        )));
    }
    if let Some(contract) = output.contracts.iter().find(|c| !listed(&c.origin)) {
        return Err(RelationError(format!(
            "relation stage `{stage}` wrote a contract with unlisted origin `{}`",
            contract.origin
        )));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use knowell_store::graph::NodeRef;
    use knowell_store::{EvidenceType, Resolution};

    fn edge(origin: &str) -> NewEdge {
        NewEdge {
            from: NodeRef::Project(ProjectId(uuid::Uuid::nil())),
            to: NodeRef::Project(ProjectId(uuid::Uuid::nil())),
            kind: "depends_on".into(),
            evidence_type: EvidenceType::Heuristic,
            resolution: Resolution::Resolved,
            evidence: serde_json::json!({}),
            origin: origin.into(),
        }
    }

    #[test]
    fn origins_must_be_prefixed_and_listed() {
        let ok = RelationOutput {
            origins: vec!["link:a.ts".into()],
            edges: vec![edge("link:a.ts")],
            contracts: Vec::new(),
        };
        assert!(validate_output("link", &ok).is_ok());
        let bad_prefix = RelationOutput {
            origins: vec!["a.ts".into()],
            ..RelationOutput::default()
        };
        assert!(validate_output("link", &bad_prefix).is_err());
        let unlisted = RelationOutput {
            origins: vec!["link:a.ts".into()],
            edges: vec![edge("link:b.ts")],
            contracts: Vec::new(),
        };
        assert!(validate_output("link", &unlisted).is_err());
        assert!(validate_output("none", &RelationOutput::default()).is_ok());
    }

    #[test]
    fn default_stage_writes_nothing() {
        let name = Name::new("p").unwrap();
        let input = RelationInput {
            workspace: WorkspaceId(uuid::Uuid::nil()),
            project: ProjectId(uuid::Uuid::nil()),
            project_name: &name,
            view: ViewId(uuid::Uuid::nil()),
            generation: 1,
            changed: &[],
            removed: &[],
        };
        assert_eq!(
            NoRelations.relate(&input).unwrap(),
            RelationOutput::default()
        );
        assert_eq!(NoRelations.name(), "none");
    }
}
