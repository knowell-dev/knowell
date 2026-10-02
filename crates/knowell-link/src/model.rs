//! Extraction results: what rule packs found in one project.

use std::collections::BTreeMap;
use std::fmt;

use knowell_core::{ContentHash, LineRange, Name, RepoPath};
use knowell_graph::{ContractKind, EdgeKind, EvidenceType};
use serde::{Deserialize, Serialize};

/// Character standing for a runtime-dynamic part of a key while it is being
/// assembled (a template interpolation, an unresolved identifier). Normalisers
/// replace it with `{}` and set [`Extraction::dynamic`].
pub(crate) const DYN: char = '\u{1}';

/// Attribute: column name contributed by one ORM column or migration step.
pub const ATTR_COLUMN: &str = "column";
/// Attribute: comma-separated, sorted column set of a table mapping.
pub const ATTR_COLUMNS: &str = "columns";
/// Attribute: migration operation (`create`, `add_column`, `drop_column`,
/// `rename_column`, `drop_table`, `rename_table`).
pub const ATTR_OP: &str = "op";
/// Attribute: new name of a renamed column or table.
pub const ATTR_NEW_NAME: &str = "new_name";
/// Attribute: entity (class / struct / model) mapping a table.
pub const ATTR_ENTITY: &str = "entity";
/// Attribute: comma-separated, sorted field names of an event schema.
pub const ATTR_FIELDS: &str = "fields";
/// Attribute: schema version of a definition (`2` for `x.v2.json`).
pub const ATTR_VERSION: &str = "version";
/// Attribute: OpenAPI operation id.
pub const ATTR_OPERATION: &str = "operation";
/// Attribute: host of an absolute client URL (`api.example.com`).
pub const ATTR_HOST: &str = "host";
/// Attribute: `true` when the key could not be resolved at all (fully
/// dynamic); the linker targets a placeholder node.
pub const ATTR_UNRESOLVED_KEY: &str = "unresolved";
/// Attribute: `all` when a wildcard key stands for every matching contract
/// (a gRPC service registration) rather than one unknown member.
pub const ATTR_GLOB: &str = "glob";

/// What the code does with a contract. Combined with the
/// [`ContractKind`] it gives the graph edge kind ([`Role::edge_kind`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Role {
    /// Serves an endpoint or RPC, publishes to a topic, writes a table,
    /// exposes a service.
    Producer,
    /// Calls an endpoint or RPC, subscribes to a topic, reads a table, an env
    /// name or an i18n key.
    Consumer,
    /// Reads a table (same as [`Role::Consumer`] for tables).
    Reads,
    /// Writes a table (same as [`Role::Producer`] for tables).
    Writes,
    /// Declares the contract: an OpenAPI operation, a proto RPC, a migration,
    /// a locale entry, an env name in deployment config.
    Definition,
}

impl Role {
    /// Stable lowercase name.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Producer => "producer",
            Self::Consumer => "consumer",
            Self::Reads => "reads",
            Self::Writes => "writes",
            Self::Definition => "definition",
        }
    }

    /// Parses [`Role::as_str`].
    pub fn parse(text: &str) -> Option<Self> {
        Some(match text {
            "producer" => Self::Producer,
            "consumer" => Self::Consumer,
            "reads" => Self::Reads,
            "writes" => Self::Writes,
            "definition" => Self::Definition,
            _ => return None,
        })
    }

    /// The graph edge kind for this role on a contract of `kind`, following
    /// the knowell-graph conventions (`client --Consumes--> endpoint`,
    /// `controller --Exposes--> endpoint`, `code --Reads--> env name`,
    /// `code --References--> i18n key`, `file --Defines--> contract`).
    pub fn edge_kind(self, kind: ContractKind) -> EdgeKind {
        match (self, kind) {
            (Self::Definition, _) => EdgeKind::Defines,
            (Self::Reads, _) => EdgeKind::Reads,
            (Self::Writes, _) => EdgeKind::Writes,
            (Self::Producer, ContractKind::Endpoint | ContractKind::Rpc | ContractKind::Infra) => {
                EdgeKind::Exposes
            }
            (Self::Producer, ContractKind::Topic) => EdgeKind::Produces,
            (Self::Producer, ContractKind::Table) => EdgeKind::Writes,
            (Self::Producer, ContractKind::EnvName | ContractKind::I18nKey) => EdgeKind::Defines,
            (Self::Producer, ContractKind::Package) => EdgeKind::Defines,
            (Self::Consumer, ContractKind::Endpoint | ContractKind::Rpc | ContractKind::Topic) => {
                EdgeKind::Consumes
            }
            (Self::Consumer, ContractKind::Table | ContractKind::EnvName) => EdgeKind::Reads,
            (Self::Consumer, ContractKind::I18nKey) => EdgeKind::References,
            (Self::Consumer, ContractKind::Package | ContractKind::Infra) => EdgeKind::DependsOn,
        }
    }

    /// Whether the role provides the contract (as opposed to using it).
    pub fn is_provider(self, kind: ContractKind) -> bool {
        matches!(
            self.edge_kind(kind),
            EdgeKind::Exposes | EdgeKind::Produces | EdgeKind::Writes
        )
    }
}

impl fmt::Display for Role {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Parses a [`ContractKind`] name as used in pack files (`endpoint`, `topic`,
/// `rpc`, `table`, `env_name`, `i18n_key`, `package`, `infra`).
pub fn parse_contract_kind(text: &str) -> Option<ContractKind> {
    Some(match text {
        "endpoint" => ContractKind::Endpoint,
        "topic" => ContractKind::Topic,
        "rpc" => ContractKind::Rpc,
        "table" => ContractKind::Table,
        "env_name" => ContractKind::EnvName,
        "i18n_key" => ContractKind::I18nKey,
        "package" => ContractKind::Package,
        "infra" => ContractKind::Infra,
        _ => return None,
    })
}

/// The code symbol an extraction belongs to.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct SymbolRef {
    /// Qualified name within the file (knowell-parse convention:
    /// `SubscriptionsController.cancel`).
    pub qualified_name: String,
    /// Lines of the symbol's declaration.
    pub range: LineRange,
}

impl SymbolRef {
    /// Graph key of the symbol: `<path>#<qualified name>` (used with
    /// [`knowell_graph::NodeId::symbol`]).
    pub fn graph_key(&self, path: &RepoPath) -> String {
        format!("{path}#{}", self.qualified_name)
    }
}

/// One contract use or declaration found in a file.
///
/// Keys are normalised per contract kind (see the crate README): endpoints
/// are `METHOD /path` with parameters as `{}`, tables are lower-case without
/// a default schema, and runtime-dynamic parts are `{}` with
/// [`Extraction::dynamic`] set. Environment variable **values** are never
/// read: for `env_name` the key is the variable name.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Extraction {
    /// Project the file belongs to.
    pub project: Name,
    /// Repository-relative path.
    pub path: RepoPath,
    /// Hash of the file version that was analysed (before redaction).
    pub content_hash: ContentHash,
    /// 1-based inclusive lines of the evidence.
    pub range: LineRange,
    /// Kind of contract.
    pub kind: ContractKind,
    /// What the code does with it.
    pub role: Role,
    /// Normalised key.
    pub key: String,
    /// The key contains runtime-dynamic parts (interpolations, unresolved
    /// identifiers, an unknown base URL); matching it is heuristic.
    pub dynamic: bool,
    /// Enclosing or captured symbol; `None` for file-level code.
    pub symbol: Option<SymbolRef>,
    /// Strength of the rule that produced it: [`EvidenceType::Syntactic`] or
    /// [`EvidenceType::Heuristic`] for code, [`EvidenceType::ContractDerived`]
    /// for contract documents (OpenAPI, AsyncAPI, proto, JSON Schema, SQL
    /// migrations).
    pub evidence: EvidenceType,
    /// Pack that produced the extraction (`name@version`).
    pub pack: String,
    /// Rule or extractor id within the pack.
    pub rule: String,
    /// Kind-specific attributes (see the `ATTR_*` constants).
    pub attrs: BTreeMap<String, String>,
}

impl Extraction {
    /// Looks up an attribute.
    pub fn attr(&self, key: &str) -> Option<&str> {
        self.attrs.get(key).map(String::as_str)
    }

    /// The edge kind this extraction becomes in the graph.
    pub fn edge_kind(&self) -> EdgeKind {
        self.role.edge_kind(self.kind)
    }

    /// Whether the key could not be resolved at all.
    pub fn is_unresolved(&self) -> bool {
        self.attr(ATTR_UNRESOLVED_KEY) == Some("true")
    }

    /// One-line, deterministic description used by pack fixture tests:
    /// `<role> <kind> <key>[ (dynamic)] @ <symbol>|<file>[ {k=v, ...}]`.
    /// Attributes holding hashes are left out.
    pub fn describe(&self) -> String {
        let mut line = format!("{} {} {}", self.role, self.kind.as_str(), self.key);
        if self.is_unresolved() {
            line.push_str(" (unresolved)");
        } else if self.dynamic {
            line.push_str(" (dynamic)");
        }
        line.push_str(" @ ");
        match &self.symbol {
            Some(symbol) => line.push_str(&symbol.qualified_name),
            None => line.push_str("<file>"),
        }
        let shown: Vec<String> = self
            .attrs
            .iter()
            .filter(|(k, _)| k.as_str() != ATTR_UNRESOLVED_KEY && !k.ends_with("hash"))
            .map(|(k, v)| format!("{k}={v}"))
            .collect();
        if !shown.is_empty() {
            line.push_str(" {");
            line.push_str(&shown.join(", "));
            line.push('}');
        }
        line
    }
}

/// A file the extractor did not analyse, with the reason.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SkippedFile {
    /// Repository-relative path.
    pub path: RepoPath,
    /// Why (`excluded: secret_file`, `unreadable`, `too_large`, ...). Never
    /// contains file content.
    pub reason: String,
}

/// The fields of a struct / class, by name, used to compare event consumers
/// with event schemas. Only names are kept.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Shape {
    /// Repository-relative path.
    pub path: RepoPath,
    /// Hash of the analysed file version.
    pub content_hash: ContentHash,
    /// The struct or class.
    pub symbol: SymbolRef,
    /// Field names as declared, sorted and unique.
    pub fields: Vec<String>,
}

/// Everything the packs found in one project.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProjectExtractions {
    /// Project name.
    pub project: Name,
    /// Extractions sorted by path, line, kind, key, role.
    pub extractions: Vec<Extraction>,
    /// Files that were not analysed, sorted by path.
    pub skipped: Vec<SkippedFile>,
    /// Struct / class field sets, sorted by path and symbol.
    pub shapes: Vec<Shape>,
    /// Packs that were active for at least one file (`name@version`), sorted.
    pub packs: Vec<String>,
}
