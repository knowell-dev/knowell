//! Types shared by several tools: the target a call acts on, evidence carried
//! by every result item, coverage gaps that explain empty or partial results,
//! and job handles for long operations.

use knowell_core::{ContentHash, LineRange, Name, RepoPath, TrackTarget};
use schemars::{JsonSchema, Schema, SchemaGenerator, json_schema};
use serde::{Deserialize, Serialize};

use crate::ids::{CommitId, ContextId, JobId, ResultId};

/// What a tool call acts on: a `context_id` from `open_workspace`
/// (preferred), or a workspace with optional per-project view pins.
///
/// Exactly one of `context_id` and `workspace` is required. The selection
/// travels with every call, so concurrent agents never change each other's
/// selection.
///
/// On the wire the view pins are one flat object mapping project name to ref
/// (`{"views": {"billing-api": "tag:v2.1.0"}}`), which keeps the schema that
/// every tool advertises small.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct Target {
    /// From open_workspace; pins every project's view.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(description = "From open_workspace")]
    pub context_id: Option<ContextId>,
    /// Workspace to use instead of `context_id`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(description = "")]
    pub workspace: Option<Name>,
    /// Project to ref (with workspace); ref is branch:x, tag:x, commit:sha or worktree.
    #[schemars(description = "project to ref (with workspace)")]
    #[serde(default, skip_serializing_if = "Vec::is_empty", with = "view_map")]
    #[schemars(schema_with = "view_map::schema")]
    pub views: Vec<ViewPin>,
}

impl Target {
    /// Targets an existing context.
    pub fn context(context_id: ContextId) -> Self {
        Self {
            context_id: Some(context_id),
            ..Self::default()
        }
    }

    /// Targets a workspace with optional view pins.
    pub fn workspace(workspace: Name, views: Vec<ViewPin>) -> Self {
        Self {
            context_id: None,
            workspace: Some(workspace),
            views,
        }
    }
}

/// Pins one project to a view for a call or a context.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
pub struct ViewPin {
    /// Project to pin.
    pub project: Name,
    /// Ref to read: `branch:<name>`, `remote:<remote>/<branch>`, `tag:<name>`, `commit:<sha>` or `worktree`.
    pub view: TrackTarget,
}

/// Wire form of view pins: one object mapping project name to ref.
///
/// The in-memory form stays a list of [`ViewPin`]; this module only changes
/// how it is (de)serialized and advertised.
pub(crate) mod view_map {
    use std::fmt;

    use knowell_core::{Name, TrackTarget};
    use schemars::{Schema, SchemaGenerator, json_schema};
    use serde::de::{Deserializer, MapAccess, Visitor};
    use serde::ser::{SerializeMap, Serializer};

    use super::ViewPin;

    /// Serializes pins as `{project: view}`.
    #[allow(clippy::ptr_arg)] // the signature serde's `with` requires
    pub(crate) fn serialize<S: Serializer>(pins: &Vec<ViewPin>, out: S) -> Result<S::Ok, S::Error> {
        let mut map = out.serialize_map(Some(pins.len()))?;
        for pin in pins {
            map.serialize_entry(&pin.project, &pin.view)?;
        }
        map.end()
    }

    /// Deserializes `{project: view}`; a non-object is rejected with a hint.
    pub(crate) fn deserialize<'de, D: Deserializer<'de>>(
        input: D,
    ) -> Result<Vec<ViewPin>, D::Error> {
        struct Pins;
        impl<'de> Visitor<'de> for Pins {
            type Value = Vec<ViewPin>;

            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str(
                    "`views` as an object mapping project name to ref, e.g. {\"api\": \"branch:main\"}",
                )
            }

            fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Self::Value, A::Error> {
                let mut pins = Vec::new();
                while let Some((project, view)) = map.next_entry::<Name, TrackTarget>()? {
                    pins.push(ViewPin { project, view });
                }
                Ok(pins)
            }
        }
        input.deserialize_map(Pins)
    }

    /// Schema of the wire form.
    pub(crate) fn schema(_: &mut SchemaGenerator) -> Schema {
        json_schema!({
            "type": "object",
            "description": "project to ref (with workspace)",
            "additionalProperties": {"type": "string"}
        })
    }
}

/// Schema of a [`LineRange`] without its prose.
pub(crate) fn lines_schema(_: &mut SchemaGenerator) -> Schema {
    json_schema!({
        "type": "object",
        "description": "1-based, inclusive",
        "properties": {"start": {"type": "integer"}, "end": {"type": "integer"}},
        "required": ["start", "end"]
    })
}

/// A file (optionally a line range) in a project, read at the view the
/// target pins for that project.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
pub struct FileLocator {
    /// Project that contains the file.
    #[schemars(description = "")]
    pub project: Name,
    /// Path relative to the project's source root.
    #[schemars(description = "")]
    pub path: RepoPath,
    /// Lines to read; the whole file when absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(schema_with = "lines_schema")]
    #[schemars(description = "")]
    pub lines: Option<LineRange>,
}

/// Whether a view is the shared index of a tracked ref or a personal layer.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ViewLayer {
    /// The shared index of the tracked ref.
    Shared,
    /// A worktree's own HEAD plus its saved, uncommitted changes.
    Personal,
}

/// How far analysis has progressed for the content behind a result
/// (architecture §6.4, tiers T0 to T3).
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize, JsonSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum FreshnessTier {
    /// T0: file text and path are searchable.
    T0Text,
    /// T1: symbols, imports and skeletons are extracted.
    T1Symbols,
    /// T2: embeddings are computed.
    T2Embeddings,
    /// T3: relations, cross-project links and staleness checks are done.
    T3Relations,
}

impl FreshnessTier {
    /// Wire name, e.g. `t2_embeddings`.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::T0Text => "t0_text",
            Self::T1Symbols => "t1_symbols",
            Self::T2Embeddings => "t2_embeddings",
            Self::T3Relations => "t3_relations",
        }
    }
}

/// State of the index that served a result.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum IndexState {
    /// Matches the latest seen commit and saved changes of the view.
    Current,
    /// Newer changes are being indexed; this is the last ready view.
    CatchingUp,
    /// Behind the latest seen commit and not being updated (paused, failed or over budget).
    Stale,
    /// No index exists yet for this project view.
    NotIndexed,
}

impl IndexState {
    /// Wire name, e.g. `catching_up`.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Current => "current",
            Self::CatchingUp => "catching_up",
            Self::Stale => "stale",
            Self::NotIndexed => "not_indexed",
        }
    }
}

/// How deeply a language is analysed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum AnalysisLevel {
    /// Text and chunking only.
    Text,
    /// tree-sitter structure: definitions, syntactic references.
    Syntactic,
    /// Compiler-grade resolution (SCIP or a language tool).
    Semantic,
}

/// Evidence carried by every source-derived result item: which exact
/// version of which lines, why it matched, and how fresh the index is.
///
/// A line number alone is not an identity: `commit`, `path` and
/// `content_hash` pin the version the lines refer to.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct Evidence {
    /// Project that contains the code.
    pub project: Name,
    /// Ref the view follows, e.g. `branch:main`.
    pub view: TrackTarget,
    /// Shared index or personal worktree layer.
    pub layer: ViewLayer,
    /// Commit of the view the lines were read from.
    pub commit: CommitId,
    /// Path relative to the project's source root.
    pub path: RepoPath,
    /// Cited lines (1-based, inclusive).
    pub lines: LineRange,
    /// BLAKE3 hash of the file version the lines refer to.
    pub content_hash: ContentHash,
    /// Enclosing symbol, when known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub symbol: Option<String>,
    /// Why this item matched or was included.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub why: Vec<MatchReason>,
    /// Analysis tier the item comes from.
    pub freshness: FreshnessTier,
    /// State of the index that served the item.
    pub index_state: IndexState,
}

/// Why a result matched. Signals are reported separately, never collapsed
/// into one confidence number.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum MatchReason {
    /// The query named this symbol exactly.
    ExactSymbol {
        /// The matched symbol.
        symbol: String,
    },
    /// The query matched this path.
    ExactPath,
    /// Query words matched (BM25).
    Lexical {
        /// Matched terms.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        terms: Vec<String>,
        /// 1-based rank among lexical candidates.
        rank: u32,
    },
    /// Similar meaning (vector search).
    Semantic {
        /// Embedding profile that produced the match.
        profile: String,
        /// 1-based rank among semantic candidates.
        rank: u32,
    },
    /// Reached over relations from another item.
    GraphPath {
        /// Hops from the starting item to this one.
        hops: Vec<GraphHop>,
    },
    /// A test references this code.
    TestReference {
        /// The referencing test.
        test: String,
    },
    /// Linked through a cross-project contract.
    Contract {
        /// Contract key, e.g. `POST /v1/subscriptions/{id}/cancel`.
        contract: String,
    },
}

/// One relation step in a graph path.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct GraphHop {
    /// Source node label (symbol or contract key).
    pub from: String,
    /// Relation followed.
    pub relation: RelationKind,
    /// Target node label.
    pub to: String,
    /// How the relation is known.
    pub evidence_type: EvidenceType,
    /// Whether the relation resolved to exactly one target.
    pub resolution: Resolution,
}

/// Kind of relation between two nodes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum RelationKind {
    /// Function or method call.
    Calls,
    /// Any other reference to a symbol.
    References,
    /// Implements an interface or trait, or overrides a method.
    Implements,
    /// Imports a module or symbol.
    Imports,
    /// Client code calls an HTTP endpoint.
    HttpCall,
    /// An HTTP endpoint is routed to its handler (edge from endpoint to handler).
    HttpRoute,
    /// Publishes to an event or topic.
    Publishes,
    /// Consumes an event or topic.
    Consumes,
    /// Calls an RPC.
    RpcCall,
    /// An RPC is served by its implementation (edge from RPC to handler).
    RpcServes,
    /// Reads a database table.
    ReadsTable,
    /// Writes a database table.
    WritesTable,
    /// Reads an environment or configuration name (never its value).
    ReadsEnv,
    /// Uses an i18n key.
    UsesI18nKey,
    /// Depends on a package.
    DependsOnPackage,
    /// A test exercises this code.
    Tests,
    /// A document or decision describes this code.
    Documents,
}

/// How a relation is known (architecture §7.1).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum EvidenceType {
    /// Verified by a compiler, SCIP or a language tool.
    SemanticallyResolved,
    /// From OpenAPI, proto, a schema or a package manifest.
    ContractDerived,
    /// Seen in source structure.
    SyntacticObservation,
    /// Name, structure or pattern similarity.
    HeuristicMatch,
    /// Proposed by a model; needs verification.
    ModelSuggestion,
    /// Observed at runtime in a specific version and environment.
    RuntimeObservation,
}

/// Whether a relation resolved to exactly one target.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum Resolution {
    /// Exactly one target.
    Resolved,
    /// Several possible targets.
    Ambiguous,
    /// Target unknown (dynamic dispatch, missing analysis, unindexed project).
    Unresolved,
}

/// A project's pinned view inside a context (one view-manifest entry).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct ProjectView {
    /// Project.
    pub project: Name,
    /// Ref the view follows.
    pub view: TrackTarget,
    /// Shared index or personal worktree layer.
    pub layer: ViewLayer,
    /// Pinned commit; absent when the ref was not found or nothing is indexed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub commit: Option<CommitId>,
    /// Generation of saved, uncommitted changes in a personal layer (0 = none).
    #[serde(default)]
    pub local_generation: u64,
    /// Highest ready analysis tier; absent when nothing is indexed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub freshness: Option<FreshnessTier>,
    /// State of the index serving this view.
    pub index_state: IndexState,
}

/// Why a result is empty or incomplete. An empty result always carries at
/// least one gap: "no result" is never presented as "does not exist".
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct Gap {
    /// Machine-readable reason.
    pub reason: GapReason,
    /// Project the gap applies to, when specific to one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project: Option<Name>,
    /// Short explanation.
    pub message: String,
}

impl Gap {
    /// Creates a gap for the whole result.
    pub fn new(reason: GapReason, message: impl Into<String>) -> Self {
        Self {
            reason,
            project: None,
            message: message.into(),
        }
    }

    /// Creates a gap specific to one project.
    pub fn for_project(reason: GapReason, project: Name, message: impl Into<String>) -> Self {
        Self {
            reason,
            project: Some(project),
            message: message.into(),
        }
    }
}

/// Machine-readable reason for an empty or incomplete result.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum GapReason {
    /// The project has no index yet.
    ProjectNotIndexed,
    /// References cannot be resolved for this language (structure-only analysis).
    NoReferenceResolutionForLanguage,
    /// Searched, but nothing matched in the selected ref.
    NoCandidatesInSelectedRef,
    /// The requested or tracked ref does not exist; no other ref was substituted.
    RefNotFound,
    /// Embeddings are not ready; semantic matches are missing.
    EmbeddingsNotReady,
    /// Relations (tier T3) are not ready; graph results are missing.
    RelationsNotReady,
    /// The language is not analysed beyond text.
    LanguageNotSupported,
    /// No rule pack recognises the framework, so contracts are missing.
    NoRulePackForFramework,
    /// The filters excluded every candidate.
    FiltersExcludedAll,
    /// Excluded by policy (sensitive path, data policy); content never read.
    ExcludedByPolicy,
    /// The requested id, path or record does not exist in the selected view.
    NotFound,
    /// The token budget ran out; more relevant items exist.
    BudgetExhausted,
    /// The result limit was reached; more items exist.
    LimitReached,
    /// A job is still computing the result; poll with the job id.
    JobPending,
    /// Every source was searched and nothing matched.
    NoMatches,
}

impl GapReason {
    /// Wire name, e.g. `project_not_indexed`.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::ProjectNotIndexed => "project_not_indexed",
            Self::NoReferenceResolutionForLanguage => "no_reference_resolution_for_language",
            Self::NoCandidatesInSelectedRef => "no_candidates_in_selected_ref",
            Self::RefNotFound => "ref_not_found",
            Self::EmbeddingsNotReady => "embeddings_not_ready",
            Self::RelationsNotReady => "relations_not_ready",
            Self::LanguageNotSupported => "language_not_supported",
            Self::NoRulePackForFramework => "no_rule_pack_for_framework",
            Self::FiltersExcludedAll => "filters_excluded_all",
            Self::ExcludedByPolicy => "excluded_by_policy",
            Self::NotFound => "not_found",
            Self::BudgetExhausted => "budget_exhausted",
            Self::LimitReached => "limit_reached",
            Self::JobPending => "job_pending",
            Self::NoMatches => "no_matches",
        }
    }
}

/// State of a job.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum JobState {
    /// Waiting to start.
    Queued,
    /// Running.
    Running,
    /// Finished; the result is available.
    Succeeded,
    /// Failed; see the message.
    Failed,
    /// Cancelled.
    Cancelled,
}

impl JobState {
    /// Whether the job will not change state any more.
    pub fn is_finished(self) -> bool {
        matches!(self, Self::Succeeded | Self::Failed | Self::Cancelled)
    }

    /// Wire name, e.g. `running`.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Queued => "queued",
            Self::Running => "running",
            Self::Succeeded => "succeeded",
            Self::Failed => "failed",
            Self::Cancelled => "cancelled",
        }
    }
}

/// Handle of a long operation returned instead of (or with) a result.
/// Call the same tool again with `job_id` to get the result.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct JobRef {
    /// Job to poll.
    pub job_id: JobId,
    /// Current state.
    pub state: JobState,
    /// Progress from 0 to 100, when known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(range(max = 100))]
    pub progress_percent: Option<u8>,
    /// Suggested wait before polling again, in milliseconds.
    pub poll_after_ms: u32,
}

/// Points at a symbol: a result id from another tool, or a name.
/// Exactly one of `id` and `symbol` is required.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct SymbolRef {
    /// Result id of a symbol or code hit.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(description = "")]
    pub id: Option<ResultId>,
    /// Symbol name, optionally qualified (A.b).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(description = "Name, optionally qualified (A.b)")]
    pub symbol: Option<String>,
    /// Disambiguates `symbol`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(description = "Disambiguates symbol")]
    pub project: Option<Name>,
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn views_travel_as_a_project_to_ref_object() {
        let target: Target = serde_json::from_value(json!({
            "workspace": "shop",
            "views": {"api": "branch:main", "web": "tag:v1"}
        }))
        .unwrap();
        assert_eq!(target.views.len(), 2);
        let back = serde_json::to_value(&target).unwrap();
        assert_eq!(
            back,
            json!({"workspace": "shop", "views": {"api": "branch:main", "web": "tag:v1"}})
        );
        let without: Target = serde_json::from_value(json!({"context_id": "c1"})).unwrap();
        assert!(without.views.is_empty());
        assert!(
            serde_json::to_value(&without)
                .unwrap()
                .get("views")
                .is_none()
        );
    }

    #[test]
    fn views_reject_other_shapes_with_a_hint() {
        for bad in [
            json!([]),
            json!("branch:main"),
            json!({"api": 1}),
            json!({"API": "branch:main"}),
        ] {
            let result =
                serde_json::from_value::<Target>(json!({"workspace": "shop", "views": bad}));
            assert!(result.is_err(), "{bad}");
        }
        let error = serde_json::from_value::<Target>(json!({"views": []}))
            .unwrap_err()
            .to_string();
        assert!(
            error.contains("object mapping project name to ref"),
            "{error}"
        );
    }
}
