//! `trace_flow`, `analyze_impact` and `contracts`.

use knowell_core::{Name, RepoPath, TrackTarget};
use schemars::{JsonSchema, Schema, SchemaGenerator, json_schema};
use serde::{Deserialize, Serialize};

use super::{
    Validate, check_len, check_limit, check_opt_text, check_range, check_unique, invalid, limits,
};
use crate::error::ToolError;
use crate::ids::{JobId, ResultId};
use crate::model::{
    Evidence, EvidenceType, Gap, JobRef, RelationKind, Resolution, SymbolRef, Target,
};

// ---------------------------------------------------------------------------
// trace_flow
// ---------------------------------------------------------------------------

/// Input of `trace_flow`. Start from `id`, `symbol` or `contract`, or pass
/// `job_id` to collect a pending trace.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct TraceFlowInput {
    /// Context or workspace.
    #[serde(flatten)]
    pub target: Target,
    /// Result id to start from.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(description = "Start: result id")]
    pub id: Option<ResultId>,
    /// Symbol to start from, optionally qualified.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(description = "Start: symbol")]
    pub symbol: Option<String>,
    /// Project that defines `symbol`, to disambiguate.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(description = "Disambiguates symbol")]
    pub project: Option<Name>,
    /// Contract key to start from, e.g. `POST /v1/subscriptions/{id}/cancel` or `topic:subscription.cancelled`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(description = "Start: contract key, e.g. POST /v1/x or topic:y")]
    pub contract: Option<String>,
    /// Direction to follow (default downstream).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(description = "default downstream")]
    pub direction: Option<FlowDirection>,
    /// Maximum hops (1-5, default 3).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(range(min = 1, max = 5))]
    #[schemars(description = "default 3")]
    pub max_depth: Option<u8>,
    /// Only these relations (all when empty).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    #[schemars(description = "")]
    pub relations: Vec<RelationKind>,
    /// Maximum nodes (default 50).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(range(min = 1, max = 200))]
    #[schemars(description = "default 50")]
    pub limit: Option<u32>,
    /// Use precision-gated, bounded navigation rather than a generic graph walk.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(description = "default false; precise routes and candidate leaves")]
    pub navigation: Option<bool>,
    /// Maximum strong neighbors expanded per node in navigation (default 12).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(range(min = 1, max = 200))]
    #[schemars(description = "default 12; navigation only")]
    pub neighbor_limit: Option<u32>,
    /// Maximum uncertain candidate edges returned per trace (default 8).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(range(min = 0, max = 200))]
    #[schemars(description = "default 8; navigation only")]
    pub candidate_limit: Option<u32>,
    /// Pending trace to collect.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(description = "Collect a pending trace")]
    pub job_id: Option<JobId>,
}

impl Validate for TraceFlowInput {
    fn validate(&self) -> Result<(), ToolError> {
        self.target.validate()?;
        let starts = [
            self.id.is_some(),
            self.symbol.is_some(),
            self.contract.is_some(),
        ]
        .into_iter()
        .filter(|set| *set)
        .count();
        match (starts, &self.job_id) {
            (0, None) => {
                return Err(invalid(
                    "pass a start (`id`, `symbol` or `contract`) or a `job_id`",
                ));
            }
            (n, _) if n > 1 => {
                return Err(invalid("pass only one of `id`, `symbol` and `contract`"));
            }
            _ => {}
        }
        check_opt_text("symbol", self.symbol.as_deref(), limits::MAX_SYMBOL_CHARS)?;
        check_opt_text(
            "contract",
            self.contract.as_deref(),
            limits::MAX_QUERY_CHARS,
        )?;
        if self.project.is_some() && self.symbol.is_none() {
            return Err(invalid("`project` only applies together with `symbol`"));
        }
        check_range("max_depth", self.max_depth, 1, limits::MAX_DEPTH)?;
        check_len("relations", self.relations.len(), limits::MAX_LIST_ITEMS)?;
        check_range("neighbor_limit", self.neighbor_limit, 1, limits::MAX_LIMIT)?;
        check_range(
            "candidate_limit",
            self.candidate_limit,
            0,
            limits::MAX_LIMIT,
        )?;
        if (self.neighbor_limit.is_some() || self.candidate_limit.is_some())
            && self.navigation != Some(true)
        {
            return Err(invalid("navigation budgets require `navigation: true`"));
        }
        check_limit(self.limit, limits::MAX_LIMIT)
    }
}

/// Direction of a trace.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum FlowDirection {
    /// What the start calls, publishes or writes.
    Downstream,
    /// What calls, consumes or reads the start.
    Upstream,
    /// Both directions.
    Both,
}

/// Output of `trace_flow`: a graph of nodes and evidenced edges.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct TraceFlowOutput {
    /// Nodes; the first one is the start.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub nodes: Vec<FlowNode>,
    /// Edges between nodes.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub edges: Vec<FlowEdge>,
    /// The trace was clipped by a node or walk budget. Navigation additionally
    /// reports omissions at depth, per-node fanout or candidate limits.
    #[serde(default)]
    pub truncated: bool,
    /// Present while the trace is still being computed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub job: Option<JobRef>,
    /// Why the result is empty or incomplete.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub gaps: Vec<Gap>,
}

/// A node in a trace.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct FlowNode {
    /// Node id, local to this result (edges refer to it).
    pub node: String,
    /// Kind of node.
    pub kind: NodeKind,
    /// Symbol name or contract key.
    pub label: String,
    /// Project, for code nodes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project: Option<Name>,
    /// Stable id of the definition; pass to fetch.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<ResultId>,
    /// Where it is defined, when it is code.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub evidence: Option<Evidence>,
}

/// Kind of a trace node.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum NodeKind {
    /// Function, method, class or other symbol.
    Symbol,
    /// HTTP endpoint.
    Endpoint,
    /// Event or message topic.
    Topic,
    /// RPC method.
    Rpc,
    /// Database table.
    Table,
    /// Environment or configuration name.
    EnvName,
    /// i18n key.
    I18nKey,
    /// Package.
    Package,
}

/// An edge in a trace. Edges point in flow direction: caller to callee,
/// client to endpoint, endpoint to handler, producer to topic to consumer.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct FlowEdge {
    /// Source node id.
    pub from: String,
    /// Target node id.
    pub to: String,
    /// Relation.
    pub relation: RelationKind,
    /// How the relation is known.
    pub evidence_type: EvidenceType,
    /// Whether it resolved to exactly one target.
    pub resolution: Resolution,
    /// Source locations supporting the edge (may be empty when unresolved).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub evidence: Vec<Evidence>,
}

// ---------------------------------------------------------------------------
// analyze_impact
// ---------------------------------------------------------------------------

/// Input of `analyze_impact`. Pass `change`, or `job_id` to collect a
/// pending analysis.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct AnalyzeImpactInput {
    /// Context or workspace.
    #[serde(flatten)]
    pub target: Target,
    /// What changes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(schema_with = "change_schema")]
    pub change: Option<ChangeSubject>,
    /// Maximum hops to follow (1-5, default 3).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(range(min = 1, max = 5))]
    #[schemars(description = "default 3")]
    pub max_depth: Option<u8>,
    /// Include tests to run (default true).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(description = "default true")]
    pub include_tests: Option<bool>,
    /// Maximum impacted items (default 50).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(range(min = 1, max = 200))]
    #[schemars(description = "default 50")]
    pub limit: Option<u32>,
    /// Pending analysis to collect.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(description = "Collect a pending analysis")]
    pub job_id: Option<JobId>,
}

/// Flat advertised form of [`ChangeSubject`]: one object with a `kind`
/// discriminator, which is exactly the wire shape of the tagged enum but far
/// smaller than four `oneOf` variants.
fn change_schema(_: &mut SchemaGenerator) -> Schema {
    json_schema!({
        "type": "object",
        "description": "symbol: id|symbol(+project); file: project+path; diff: project+base(+head); patch: project+patch (unified diff)",
        "properties": {
            "kind": {"type": "string", "enum": ["symbol", "file", "diff", "patch"]},
            "id": {"type": "string"},
            "symbol": {"type": "string"},
            "project": {"type": "string"},
            "path": {"type": "string"},
            "base": {"type": "string"},
            "head": {"type": "string"},
            "patch": {"type": "string"}
        },
        "required": ["kind"],
        "additionalProperties": false
    })
}

/// What `analyze_impact` analyses.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ChangeSubject {
    /// A symbol (by result id or name).
    Symbol {
        /// The symbol.
        #[serde(flatten)]
        symbol: SymbolRef,
    },
    /// Every symbol in a file.
    File {
        /// Project.
        project: Name,
        /// File path.
        path: RepoPath,
    },
    /// Committed changes between two refs of a project.
    Diff {
        /// Project.
        project: Name,
        /// Base ref.
        base: TrackTarget,
        /// Head ref; the context's view of the project (with saved changes) when absent.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        head: Option<TrackTarget>,
    },
    /// An unapplied unified diff, analysed in a temporary view.
    Patch {
        /// Project the patch applies to.
        project: Name,
        /// Unified diff text (at most 1 MiB).
        patch: String,
    },
}

impl Validate for AnalyzeImpactInput {
    fn validate(&self) -> Result<(), ToolError> {
        self.target.validate()?;
        match (&self.change, &self.job_id) {
            (None, None) => return Err(invalid("pass `change` or a `job_id`")),
            (Some(_), Some(_)) => {
                return Err(invalid("pass either `change` or `job_id`, not both"));
            }
            _ => {}
        }
        match &self.change {
            Some(ChangeSubject::Symbol { symbol }) => symbol.validate()?,
            Some(ChangeSubject::Patch { patch, .. }) => {
                if patch.trim().is_empty() {
                    return Err(invalid("`patch` must not be empty"));
                }
                if patch.len() > limits::MAX_PATCH_BYTES {
                    return Err(invalid(format!(
                        "`patch` is larger than {} bytes",
                        limits::MAX_PATCH_BYTES
                    )));
                }
                if patch.contains('\0') {
                    return Err(invalid("`patch` must not contain NUL characters"));
                }
            }
            Some(ChangeSubject::Diff { base, head, .. }) => {
                if head.as_ref() == Some(base) {
                    return Err(invalid("`base` and `head` are the same ref"));
                }
            }
            Some(ChangeSubject::File { .. }) | None => {}
        }
        check_range("max_depth", self.max_depth, 1, limits::MAX_DEPTH)?;
        check_limit(self.limit, limits::MAX_LIMIT)
    }
}

/// Output of `analyze_impact`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct AnalyzeImpactOutput {
    /// Short description of what was analysed.
    pub subject: String,
    /// Symbols, files and contracts changed directly (distance 0).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub changed: Vec<ImpactItem>,
    /// Items affected through relations, nearest first.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub impacted: Vec<ImpactItem>,
    /// Tests to run.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tests: Vec<ImpactItem>,
    /// Risk with the factors behind it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub risk: Option<Risk>,
    /// The result was cut by `max_depth` or `limit`.
    #[serde(default)]
    pub truncated: bool,
    /// Present while the analysis is still running.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub job: Option<JobRef>,
    /// Why the result is empty or incomplete.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub gaps: Vec<Gap>,
}

/// An item touched by a change.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct ImpactItem {
    /// Stable id; pass to fetch.
    pub id: ResultId,
    /// Kind of item.
    pub kind: ImpactKind,
    /// Symbol name, path or contract key.
    pub name: String,
    /// Hops from the change (0 = changed directly).
    pub distance: u8,
    /// Location, and the graph path in `why`.
    pub evidence: Evidence,
}

/// Kind of an impacted item.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ImpactKind {
    /// Symbol.
    Symbol,
    /// File.
    File,
    /// Contract (endpoint, topic, RPC, table, …).
    Contract,
    /// Test.
    Test,
    /// Configuration.
    Config,
}

/// Risk of a change, always explained by its factors.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct Risk {
    /// Overall level.
    pub level: RiskLevel,
    /// Factors behind the level.
    pub factors: Vec<RiskFactor>,
}

/// Risk level.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize, JsonSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum RiskLevel {
    /// Contained change.
    Low,
    /// Some dependents or missing evidence.
    Medium,
    /// Public contracts or many dependents.
    High,
}

/// One factor contributing to risk.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct RiskFactor {
    /// Machine-readable factor.
    pub code: RiskCode,
    /// Short explanation.
    pub message: String,
    /// Supporting locations.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub evidence: Vec<Evidence>,
}

/// Machine-readable risk factor.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum RiskCode {
    /// A public contract changes.
    PublicContractChanged,
    /// Other projects consume the changed code.
    CrossProjectConsumers,
    /// The signature changes incompatibly.
    BreakingSignatureChange,
    /// Changed code has no test evidence.
    UntestedCode,
    /// Some references could not be resolved; impact may be larger.
    UnresolvedReferences,
    /// A schema migration is needed.
    MigrationRequired,
    /// Many dependents.
    ManyDependents,
}

// ---------------------------------------------------------------------------
// contracts
// ---------------------------------------------------------------------------

/// Input of `contracts`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct ContractsInput {
    /// Context or workspace.
    #[serde(flatten)]
    pub target: Target,
    /// Filter by key text, e.g. `subscriptions` or `payment.completed`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(description = "Key text, e.g. subscriptions")]
    pub query: Option<String>,
    /// Only these kinds (all when empty).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    #[schemars(description = "")]
    pub kinds: Vec<ContractKind>,
    /// Only contracts this project takes part in.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(description = "Only contracts it takes part in")]
    pub project: Option<Name>,
    /// Only contracts with drift findings (default false).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(description = "")]
    pub only_drift: Option<bool>,
    /// Maximum contracts (default 20).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(range(min = 1, max = 200))]
    #[schemars(description = "default 20")]
    pub limit: Option<u32>,
}

impl Validate for ContractsInput {
    fn validate(&self) -> Result<(), ToolError> {
        self.target.validate()?;
        check_opt_text("query", self.query.as_deref(), limits::MAX_QUERY_CHARS)?;
        check_len("kinds", self.kinds.len(), limits::MAX_LIST_ITEMS)?;
        check_unique("kinds", &self.kinds)?;
        check_limit(self.limit, limits::MAX_LIMIT)
    }
}

/// Kind of cross-project contract.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize, JsonSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum ContractKind {
    /// HTTP endpoint.
    Endpoint,
    /// Event or message topic.
    Topic,
    /// RPC method.
    Rpc,
    /// Database table.
    Table,
    /// Environment or configuration name (never its value).
    EnvName,
    /// i18n key.
    I18nKey,
    /// Package.
    Package,
}

/// Output of `contracts`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct ContractsOutput {
    /// Contracts, sorted by kind then key.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub contracts: Vec<ContractInfo>,
    /// More contracts exist beyond `limit`.
    #[serde(default)]
    pub more_available: bool,
    /// Why the result is empty or incomplete.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub gaps: Vec<Gap>,
}

/// A contract and the code on each side of it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct ContractInfo {
    /// Stable id of the contract itself (not a source range; fetch the
    /// participants' evidence instead).
    pub id: ResultId,
    /// Kind.
    pub kind: ContractKind,
    /// Key, e.g. `POST /v1/subscriptions/{id}/cancel`.
    pub key: String,
    /// Definitions, producers and consumers.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub participants: Vec<ContractParticipant>,
    /// Drift findings.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub drift: Vec<DriftFinding>,
}

/// One side of a contract.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct ContractParticipant {
    /// Project.
    pub project: Name,
    /// Role in the contract.
    pub role: ContractRole,
    /// How the participation is known.
    pub evidence_type: EvidenceType,
    /// Whether it resolved to exactly this contract.
    pub resolution: Resolution,
    /// Where it is.
    pub evidence: Evidence,
}

/// Role of a participant in a contract.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ContractRole {
    /// Schema or specification (OpenAPI, proto, migration).
    Definition,
    /// Serves or publishes.
    Producer,
    /// Calls or consumes.
    Consumer,
    /// Reads (tables, env names, i18n keys).
    Reader,
    /// Writes (tables).
    Writer,
}

/// A drift finding on a contract.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct DriftFinding {
    /// Machine-readable finding.
    pub code: DriftCode,
    /// Short explanation.
    pub message: String,
    /// Supporting locations.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub evidence: Vec<Evidence>,
}

/// Machine-readable drift finding (architecture §8 graph insights).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum DriftCode {
    /// A consumer uses an older schema than the producer.
    ConsumerOnOldSchema,
    /// An endpoint with no known client.
    EndpointWithoutClient,
    /// An event with no known consumer (in analysed projects).
    EventWithoutConsumer,
    /// A table that is written but never read.
    TableNeverRead,
    /// A migration does not match the entity definition.
    MigrationEntityMismatch,
    /// An i18n key is used but missing from a language file.
    MissingI18nKey,
    /// Clients of the same contract differ (web, mobile, …).
    ClientParityGap,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ids::ContextId;

    fn target() -> Target {
        Target::context(ContextId::new("ctx-1").unwrap())
    }

    #[test]
    fn trace_needs_one_start_or_a_job() {
        let mut input = TraceFlowInput {
            target: target(),
            ..TraceFlowInput::default()
        };
        assert!(input.validate().is_err());
        input.symbol = Some("PaymentService".into());
        assert!(input.validate().is_ok());
        input.contract = Some("topic:x".into());
        assert!(input.validate().is_err());
        input.contract = None;
        input.max_depth = Some(6);
        assert!(input.validate().is_err());
        let job = TraceFlowInput {
            target: target(),
            job_id: Some(JobId::new("job-1").unwrap()),
            ..TraceFlowInput::default()
        };
        assert!(job.validate().is_ok());
        let stray_project = TraceFlowInput {
            target: target(),
            contract: Some("topic:x".into()),
            project: Some(Name::new("api").unwrap()),
            ..TraceFlowInput::default()
        };
        assert!(stray_project.validate().is_err());
    }

    #[test]
    fn impact_patch_limits() {
        let project = Name::new("api").unwrap();
        let mut input = AnalyzeImpactInput {
            target: target(),
            change: Some(ChangeSubject::Patch {
                project: project.clone(),
                patch: "  ".into(),
            }),
            ..AnalyzeImpactInput::default()
        };
        assert!(input.validate().is_err(), "blank patch");
        input.change = Some(ChangeSubject::Patch {
            project: project.clone(),
            patch: "x".repeat(limits::MAX_PATCH_BYTES + 1),
        });
        assert!(input.validate().is_err(), "oversized patch");
        input.change = Some(ChangeSubject::Patch {
            project: project.clone(),
            patch: "--- a/x\n+++ b/x\n@@ -1 +1 @@\n-a\n+b\n".into(),
        });
        assert!(input.validate().is_ok());
        input.job_id = Some(JobId::new("job-1").unwrap());
        assert!(input.validate().is_err(), "change and job_id together");
        let same = AnalyzeImpactInput {
            target: target(),
            change: Some(ChangeSubject::Diff {
                project,
                base: "branch:main".parse().unwrap(),
                head: Some("branch:main".parse().unwrap()),
            }),
            ..AnalyzeImpactInput::default()
        };
        assert!(same.validate().is_err());
    }

    #[test]
    fn change_subject_wire_format() {
        let json = r#"{"kind":"symbol","symbol":"PaymentService.cancelSubscription","project":"billing-api"}"#;
        let subject: ChangeSubject = serde_json::from_str(json).unwrap();
        assert!(matches!(subject, ChangeSubject::Symbol { ref symbol } if symbol.symbol.is_some()));
        assert!(serde_json::from_str::<ChangeSubject>(r#"{"kind":"rewrite"}"#).is_err());
        assert!(
            serde_json::from_str::<ChangeSubject>(
                r#"{"kind":"file","project":"api","path":"../x"}"#
            )
            .is_err()
        );
    }
}
