//! Validated plugin input and output types.
//!
//! Enum names match `knowell-graph`'s (`as_str` gives the same snake_case
//! spelling), so the indexer can map plugin output onto graph nodes and
//! edges without a translation table.

use std::collections::BTreeSet;

use knowell_core::{LineRange, Name, RepoPath};
use serde::{Deserialize, Serialize};

use crate::manifest::Capability;

/// Longest contract key or symbol key a plugin may return, in bytes.
pub const MAX_KEY_BYTES: usize = 512;

/// Output accounting charges this many bytes per contract or edge on top of
/// its strings, so a flood of empty items is bounded too.
pub const ITEM_OVERHEAD_BYTES: u64 = 32;

/// The file handed to a plugin.
#[derive(Debug, Clone, Copy)]
pub struct SourceFile<'a> {
    /// Path relative to the project root.
    pub path: &'a RepoPath,
    /// Language identifier (must be one of the plugin's languages).
    pub language: &'a str,
    /// Full file text. The caller has already excluded sensitive files and
    /// redacted secrets; the plugin sees exactly this text.
    pub text: &'a str,
}

/// What a contract stands for.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ContractKind {
    /// An HTTP endpoint (`GET /v1/invoices/{id}`).
    Endpoint,
    /// An event topic or queue.
    Topic,
    /// An RPC method.
    Rpc,
    /// A database table.
    Table,
    /// An environment variable or config name (never its value).
    EnvName,
    /// An i18n message key.
    I18nKey,
    /// A package or library.
    Package,
}

impl ContractKind {
    /// Stable snake_case name (same as `knowell-graph`).
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Endpoint => "endpoint",
            Self::Topic => "topic",
            Self::Rpc => "rpc",
            Self::Table => "table",
            Self::EnvName => "env_name",
            Self::I18nKey => "i18n_key",
            Self::Package => "package",
        }
    }
}

/// Which side of a contract the code is on.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ContractRole {
    /// Defines, serves or publishes the contract.
    Producer,
    /// Calls, reads or subscribes to the contract.
    Consumer,
}

impl ContractRole {
    /// Stable snake_case name.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Producer => "producer",
            Self::Consumer => "consumer",
        }
    }
}

/// Relationship between two symbols; `a --kind--> b` reads "a *kind* b".
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EdgeKind {
    /// Symbol calls symbol.
    Calls,
    /// Symbol refers to another without calling it.
    References,
    /// Symbol implements an interface/trait.
    Implements,
    /// File or symbol imports another.
    Imports,
    /// File or symbol defines a symbol or contract.
    Defines,
    /// Test covers a symbol or contract.
    Tests,
    /// Code produces an event/message on a contract.
    Produces,
    /// Code consumes a contract (calls an endpoint, subscribes to a topic).
    Consumes,
    /// Code reads a table, env name or i18n key.
    Reads,
    /// Code writes a table.
    Writes,
    /// Code serves an endpoint or RPC.
    Exposes,
    /// Code depends on a package or service.
    DependsOn,
}

impl EdgeKind {
    /// Stable snake_case name (same as `knowell-graph`).
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Calls => "calls",
            Self::References => "references",
            Self::Implements => "implements",
            Self::Imports => "imports",
            Self::Defines => "defines",
            Self::Tests => "tests",
            Self::Produces => "produces",
            Self::Consumes => "consumes",
            Self::Reads => "reads",
            Self::Writes => "writes",
            Self::Exposes => "exposes",
            Self::DependsOn => "depends_on",
        }
    }
}

/// How a plugin established an edge. Plugins can only claim the two weakest
/// evidence types; semantic resolution comes from compilers and SCIP.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Evidence {
    /// Seen in the source structure.
    Syntactic,
    /// Name, structure or pattern similarity.
    Heuristic,
}

impl Evidence {
    /// Stable snake_case name (same as `knowell-graph`'s `EvidenceType`).
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Syntactic => "syntactic",
            Self::Heuristic => "heuristic",
        }
    }
}

/// Whether an edge's target is known.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Resolution {
    /// Exactly one target.
    Resolved,
    /// Several equally plausible targets.
    Ambiguous,
    /// The target could not be determined.
    Unresolved,
}

impl Resolution {
    /// Stable snake_case name (same as `knowell-graph`).
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Resolved => "resolved",
            Self::Ambiguous => "ambiguous",
            Self::Unresolved => "unresolved",
        }
    }
}

/// A contract occurrence in the analysed file.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Contract {
    /// What the contract stands for.
    pub kind: ContractKind,
    /// Normalised key: 1-[`MAX_KEY_BYTES`] bytes, not blank, no control characters.
    pub key: String,
    /// Producer or consumer side.
    pub role: ContractRole,
    /// Lines in the analysed file (within the file's line count).
    pub range: LineRange,
}

/// A relation between two symbol (or contract) keys.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Edge {
    /// Source key; same rules as [`Contract::key`].
    pub from: String,
    /// Target key; same rules as [`Contract::key`].
    pub to: String,
    /// Relationship.
    pub kind: EdgeKind,
    /// How it was established.
    pub evidence: Evidence,
    /// Whether the target is known.
    pub resolution: Resolution,
    /// Lines in the analysed file where the relation is observed.
    pub range: LineRange,
}

/// Validated output of one `analyze` call, in the order the plugin returned it.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct AnalyzerOutput {
    /// Contract occurrences.
    pub contracts: Vec<Contract>,
    /// Relations.
    pub edges: Vec<Edge>,
}

/// A plugin's validated self-description.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PluginInfo {
    pub(crate) name: Name,
    pub(crate) version: String,
    pub(crate) languages: BTreeSet<String>,
    pub(crate) frameworks: BTreeSet<String>,
    pub(crate) capabilities: BTreeSet<Capability>,
}

impl PluginInfo {
    /// Plugin name (equals the manifest name).
    pub fn name(&self) -> &Name {
        &self.name
    }

    /// Plugin version (equals the manifest version).
    pub fn version(&self) -> &str {
        &self.version
    }

    /// Languages the plugin analyses, sorted.
    pub fn languages(&self) -> &BTreeSet<String> {
        &self.languages
    }

    /// Frameworks the plugin recognises, sorted.
    pub fn frameworks(&self) -> &BTreeSet<String> {
        &self.frameworks
    }

    /// Capabilities the plugin declared it uses (a subset of the manifest's).
    pub fn capabilities(&self) -> &BTreeSet<Capability> {
        &self.capabilities
    }
}

/// Checks a contract or symbol key; returns the reason it is invalid.
pub(crate) fn key_problem(key: &str) -> Option<&'static str> {
    if key.trim().is_empty() {
        Some("is empty or blank")
    } else if key.len() > MAX_KEY_BYTES {
        Some("is longer than 512 bytes")
    } else if key.chars().any(char::is_control) {
        Some("contains control characters")
    } else {
        None
    }
}

/// Builds a [`LineRange`] that must lie within a file of `line_count` lines.
pub(crate) fn checked_range(start: u32, end: u32, line_count: u32) -> Result<LineRange, String> {
    let range = LineRange::new(start, end).map_err(|e| format!("line range {e}"))?;
    if range.end() > line_count {
        return Err(format!(
            "line range {range} is outside the file, which has {line_count} lines"
        ));
    }
    Ok(range)
}

/// Number of lines in `text`, counting a final line without a newline.
pub(crate) fn line_count(text: &str) -> u32 {
    u32::try_from(text.lines().count()).unwrap_or(u32::MAX)
}

/// Whether `id` is a valid language or framework identifier: 1-64 bytes from
/// `[a-z0-9+#._-]`, starting with a letter or digit.
pub(crate) fn is_identifier(id: &str) -> bool {
    let mut bytes = id.bytes();
    let first_ok = bytes
        .next()
        .is_some_and(|b| b.is_ascii_lowercase() || b.is_ascii_digit());
    first_ok
        && id.len() <= 64
        && bytes.all(|b| {
            b.is_ascii_lowercase()
                || b.is_ascii_digit()
                || matches!(b, b'+' | b'#' | b'.' | b'_' | b'-')
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keys() {
        assert_eq!(key_problem("GET /users/{id}"), None);
        assert_eq!(key_problem("ünïcode.key"), None);
        assert!(key_problem("").is_some());
        assert!(key_problem("   ").is_some());
        assert!(key_problem("a\nb").is_some());
        assert!(key_problem("a\u{0}b").is_some());
        assert!(key_problem(&"k".repeat(MAX_KEY_BYTES)).is_none());
        assert!(key_problem(&"k".repeat(MAX_KEY_BYTES + 1)).is_some());
    }

    #[test]
    fn ranges() {
        assert_eq!(
            checked_range(1, 3, 3).unwrap(),
            LineRange::new(1, 3).unwrap()
        );
        assert!(checked_range(0, 1, 3).is_err());
        assert!(checked_range(2, 1, 3).is_err());
        assert!(checked_range(1, 4, 3).is_err());
        assert!(
            checked_range(1, 1, 0).is_err(),
            "an empty file has no lines"
        );
    }

    #[test]
    fn counts_lines() {
        assert_eq!(line_count(""), 0);
        assert_eq!(line_count("a"), 1);
        assert_eq!(line_count("a\n"), 1);
        assert_eq!(line_count("a\nb"), 2);
        assert_eq!(line_count("a\r\nb\r\n"), 2);
        assert_eq!(line_count("\n\n"), 2);
    }

    #[test]
    fn identifiers() {
        for ok in ["toy", "c++", "c#", "objective-c", "vue.js", "go_1", "3d"] {
            assert!(is_identifier(ok), "{ok}");
        }
        let long = "a".repeat(65);
        for bad in ["", "Toy", "-x", ".x", "a b", "ä", long.as_str()] {
            assert!(!is_identifier(bad), "{bad}");
        }
    }

    #[test]
    fn names_match_the_graph_spelling() {
        assert_eq!(ContractKind::EnvName.as_str(), "env_name");
        assert_eq!(ContractKind::I18nKey.as_str(), "i18n_key");
        assert_eq!(EdgeKind::DependsOn.as_str(), "depends_on");
        assert_eq!(ContractRole::Consumer.as_str(), "consumer");
        assert_eq!(Evidence::Heuristic.as_str(), "heuristic");
        assert_eq!(Resolution::Ambiguous.as_str(), "ambiguous");
    }
}
