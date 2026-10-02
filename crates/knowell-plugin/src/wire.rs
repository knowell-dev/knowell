//! Component-model mirrors of the WIT types in `wit/plugin.wit`, and the
//! conversion of untrusted plugin values into validated host types.
//!
//! Results are copied out of the plugin during the call (Wasmtime runs the
//! guest's post-return cleanup before `call` returns, so nothing may point
//! into guest memory afterwards). The copy is bounded *before* it happens by
//! the store's hostcall fuel, which [`hostcall_fuel`] derives from the output
//! limit; the exact output accounting and all content checks run afterwards.
//!
//! Wasmtime type-checks these definitions against the component when a plugin
//! is loaded, so drift between this file and the WIT is a load error.

use std::collections::BTreeSet;

use wasmtime::component::{ComponentType, Lift, Lower};

use crate::error::{DeclineKind, PluginError};
use crate::manifest::{Capability, Manifest};
use crate::output::{
    AnalyzerOutput, Contract, ContractKind, ContractRole, Edge, EdgeKind, Evidence,
    ITEM_OVERHEAD_BYTES, PluginInfo, Resolution, checked_range, is_identifier, key_problem,
};
use crate::sanitize::sanitize;

/// Longest decline message kept, in bytes.
const MAX_DECLINE_MESSAGE_BYTES: usize = 256;
/// Bounds on metadata lists and strings.
const MAX_INFO_ENTRIES: usize = 64;
const MAX_INFO_TEXT_BYTES: usize = 64;
/// Hostcall fuel never drops below this, so ordinary log messages and paths
/// always fit even with a tiny output limit.
const MIN_HOSTCALL_FUEL: u64 = 1024 * 1024;

/// Bytes the host may copy out of a plugin in one transfer (a result or one
/// import call's arguments). Wasmtime charges string bytes plus the host-side
/// size of every list element (at most 64 bytes for a contract or edge, twice
/// the per-item accounting), so twice the output limit admits every output
/// within the limit while capping host allocation for any larger one.
pub(crate) fn hostcall_fuel(max_output_bytes: u64) -> usize {
    let fuel = max_output_bytes.saturating_mul(2).max(MIN_HOSTCALL_FUEL);
    usize::try_from(fuel).unwrap_or(usize::MAX)
}

// ---- analyzer -------------------------------------------------------------

#[derive(ComponentType, Lower)]
#[component(record)]
pub(crate) struct WireSourceFile<'a> {
    #[component(name = "path")]
    pub(crate) path: &'a str,
    #[component(name = "language")]
    pub(crate) language: &'a str,
    #[component(name = "text")]
    pub(crate) text: &'a str,
}

#[derive(ComponentType, Lift, Clone, Copy, Debug)]
#[component(record)]
pub(crate) struct WireLineRange {
    #[component(name = "start")]
    pub(crate) start: u32,
    #[component(name = "end")]
    pub(crate) end: u32,
}

#[allow(
    dead_code,
    reason = "variants are only constructed by the component-model lift"
)]
#[derive(ComponentType, Lift, Clone, Copy, Debug)]
#[component(enum)]
#[repr(u8)]
pub(crate) enum WireContractKind {
    #[component(name = "endpoint")]
    Endpoint,
    #[component(name = "topic")]
    Topic,
    #[component(name = "rpc")]
    Rpc,
    #[component(name = "table")]
    Table,
    #[component(name = "env-name")]
    EnvName,
    #[component(name = "i18n-key")]
    I18nKey,
    #[component(name = "package")]
    Package,
}

#[allow(
    dead_code,
    reason = "variants are only constructed by the component-model lift"
)]
#[derive(ComponentType, Lift, Clone, Copy, Debug)]
#[component(enum)]
#[repr(u8)]
pub(crate) enum WireContractRole {
    #[component(name = "producer")]
    Producer,
    #[component(name = "consumer")]
    Consumer,
}

#[derive(ComponentType, Lift, Debug)]
#[component(record)]
pub(crate) struct WireContract {
    #[component(name = "kind")]
    pub(crate) kind: WireContractKind,
    #[component(name = "key")]
    pub(crate) key: String,
    #[component(name = "role")]
    pub(crate) role: WireContractRole,
    #[component(name = "range")]
    pub(crate) range: WireLineRange,
}

#[allow(
    dead_code,
    reason = "variants are only constructed by the component-model lift"
)]
#[derive(ComponentType, Lift, Clone, Copy, Debug)]
#[component(enum)]
#[repr(u8)]
pub(crate) enum WireEdgeKind {
    #[component(name = "calls")]
    Calls,
    #[component(name = "references")]
    References,
    #[component(name = "implements")]
    Implements,
    #[component(name = "imports")]
    Imports,
    #[component(name = "defines")]
    Defines,
    #[component(name = "tests")]
    Tests,
    #[component(name = "produces")]
    Produces,
    #[component(name = "consumes")]
    Consumes,
    #[component(name = "reads")]
    Reads,
    #[component(name = "writes")]
    Writes,
    #[component(name = "exposes")]
    Exposes,
    #[component(name = "depends-on")]
    DependsOn,
}

#[allow(
    dead_code,
    reason = "variants are only constructed by the component-model lift"
)]
#[derive(ComponentType, Lift, Clone, Copy, Debug)]
#[component(enum)]
#[repr(u8)]
pub(crate) enum WireEvidence {
    #[component(name = "syntactic")]
    Syntactic,
    #[component(name = "heuristic")]
    Heuristic,
}

#[allow(
    dead_code,
    reason = "variants are only constructed by the component-model lift"
)]
#[derive(ComponentType, Lift, Clone, Copy, Debug)]
#[component(enum)]
#[repr(u8)]
pub(crate) enum WireResolution {
    #[component(name = "resolved")]
    Resolved,
    #[component(name = "ambiguous")]
    Ambiguous,
    #[component(name = "unresolved")]
    Unresolved,
}

#[derive(ComponentType, Lift, Debug)]
#[component(record)]
pub(crate) struct WireEdge {
    #[component(name = "from-symbol")]
    pub(crate) from_symbol: String,
    #[component(name = "to-symbol")]
    pub(crate) to_symbol: String,
    #[component(name = "kind")]
    pub(crate) kind: WireEdgeKind,
    #[component(name = "evidence")]
    pub(crate) evidence: WireEvidence,
    #[component(name = "resolution")]
    pub(crate) resolution: WireResolution,
    #[component(name = "range")]
    pub(crate) range: WireLineRange,
}

#[derive(ComponentType, Lift, Debug)]
#[component(record)]
pub(crate) struct WireAnalysis {
    #[component(name = "contracts")]
    pub(crate) contracts: Vec<WireContract>,
    #[component(name = "edges")]
    pub(crate) edges: Vec<WireEdge>,
}

#[derive(ComponentType, Lift, Debug)]
#[component(variant)]
pub(crate) enum WireAnalyzeError {
    #[component(name = "unsupported")]
    Unsupported(String),
    #[component(name = "failed")]
    Failed(String),
}

/// Params and results of `analyzer.analyze`.
pub(crate) type AnalyzeParams<'a> = (WireSourceFile<'a>,);
pub(crate) type AnalyzeResults = (Result<WireAnalysis, WireAnalyzeError>,);

// ---- metadata -------------------------------------------------------------

#[allow(
    dead_code,
    reason = "variants are only constructed by the component-model lift"
)]
#[derive(ComponentType, Lift, Clone, Copy, Debug)]
#[component(enum)]
#[repr(u8)]
pub(crate) enum WireCapability {
    #[component(name = "project-files")]
    ProjectFiles,
}

#[derive(ComponentType, Lift, Debug)]
#[component(record)]
pub(crate) struct WirePluginInfo {
    #[component(name = "name")]
    pub(crate) name: String,
    #[component(name = "version")]
    pub(crate) version: String,
    #[component(name = "languages")]
    pub(crate) languages: Vec<String>,
    #[component(name = "frameworks")]
    pub(crate) frameworks: Vec<String>,
    #[component(name = "capabilities")]
    pub(crate) capabilities: Vec<WireCapability>,
}

/// Results of `metadata.info`.
pub(crate) type InfoResults = (WirePluginInfo,);

// ---- host imports ---------------------------------------------------------

#[allow(
    dead_code,
    reason = "variants are only constructed by the component-model lift"
)]
#[derive(ComponentType, Lift, Clone, Copy, Debug, PartialEq, Eq)]
#[component(enum)]
#[repr(u8)]
pub(crate) enum WireLogLevel {
    #[component(name = "trace")]
    Trace,
    #[component(name = "debug")]
    Debug,
    #[component(name = "info")]
    Info,
    #[component(name = "warn")]
    Warn,
    #[component(name = "error")]
    Error,
}

#[derive(ComponentType, Lower, Clone, Copy, Debug, PartialEq, Eq)]
#[component(enum)]
#[repr(u8)]
pub(crate) enum WireReadError {
    #[component(name = "denied")]
    Denied,
    #[component(name = "not-found")]
    NotFound,
    #[component(name = "too-large")]
    TooLarge,
    #[component(name = "not-text")]
    NotText,
    #[component(name = "budget-exhausted")]
    BudgetExhausted,
    #[component(name = "unavailable")]
    Unavailable,
}

// ---- conversions ----------------------------------------------------------

impl From<WireContractKind> for ContractKind {
    fn from(kind: WireContractKind) -> Self {
        match kind {
            WireContractKind::Endpoint => Self::Endpoint,
            WireContractKind::Topic => Self::Topic,
            WireContractKind::Rpc => Self::Rpc,
            WireContractKind::Table => Self::Table,
            WireContractKind::EnvName => Self::EnvName,
            WireContractKind::I18nKey => Self::I18nKey,
            WireContractKind::Package => Self::Package,
        }
    }
}

impl From<WireContractRole> for ContractRole {
    fn from(role: WireContractRole) -> Self {
        match role {
            WireContractRole::Producer => Self::Producer,
            WireContractRole::Consumer => Self::Consumer,
        }
    }
}

impl From<WireEdgeKind> for EdgeKind {
    fn from(kind: WireEdgeKind) -> Self {
        match kind {
            WireEdgeKind::Calls => Self::Calls,
            WireEdgeKind::References => Self::References,
            WireEdgeKind::Implements => Self::Implements,
            WireEdgeKind::Imports => Self::Imports,
            WireEdgeKind::Defines => Self::Defines,
            WireEdgeKind::Tests => Self::Tests,
            WireEdgeKind::Produces => Self::Produces,
            WireEdgeKind::Consumes => Self::Consumes,
            WireEdgeKind::Reads => Self::Reads,
            WireEdgeKind::Writes => Self::Writes,
            WireEdgeKind::Exposes => Self::Exposes,
            WireEdgeKind::DependsOn => Self::DependsOn,
        }
    }
}

impl From<WireEvidence> for Evidence {
    fn from(evidence: WireEvidence) -> Self {
        match evidence {
            WireEvidence::Syntactic => Self::Syntactic,
            WireEvidence::Heuristic => Self::Heuristic,
        }
    }
}

impl From<WireResolution> for Resolution {
    fn from(resolution: WireResolution) -> Self {
        match resolution {
            WireResolution::Resolved => Self::Resolved,
            WireResolution::Ambiguous => Self::Ambiguous,
            WireResolution::Unresolved => Self::Unresolved,
        }
    }
}

impl From<WireCapability> for Capability {
    fn from(capability: WireCapability) -> Self {
        match capability {
            WireCapability::ProjectFiles => Self::ProjectFiles,
        }
    }
}

// ---- validation -----------------------------------------------------------

fn len_u64(len: usize) -> u64 {
    u64::try_from(len).unwrap_or(u64::MAX)
}

/// Bytes `raw` counts against the output limit: every string plus
/// [`ITEM_OVERHEAD_BYTES`] per contract or edge.
fn output_size(raw: &WireAnalysis) -> u64 {
    let contracts = raw.contracts.iter().fold(0u64, |sum, c| {
        sum.saturating_add(ITEM_OVERHEAD_BYTES)
            .saturating_add(len_u64(c.key.len()))
    });
    raw.edges.iter().fold(contracts, |sum, e| {
        sum.saturating_add(ITEM_OVERHEAD_BYTES)
            .saturating_add(len_u64(e.from_symbol.len()))
            .saturating_add(len_u64(e.to_symbol.len()))
    })
}

/// Validates the plugin's `analysis`: total size, every key, every range
/// against the file's `line_count`. Enum values were validated by the lift.
pub(crate) fn validate_analysis(
    raw: WireAnalysis,
    plugin: &str,
    max_output_bytes: u64,
    line_count: u32,
) -> Result<AnalyzerOutput, PluginError> {
    if output_size(&raw) > max_output_bytes {
        return Err(PluginError::OutputTooLarge {
            plugin: plugin.to_string(),
            limit: max_output_bytes,
        });
    }
    let invalid = |reason: String| PluginError::InvalidOutput {
        plugin: plugin.to_string(),
        reason,
    };
    let mut output = AnalyzerOutput {
        contracts: Vec::with_capacity(raw.contracts.len()),
        edges: Vec::with_capacity(raw.edges.len()),
    };
    for (index, item) in raw.contracts.into_iter().enumerate() {
        if let Some(problem) = key_problem(&item.key) {
            return Err(invalid(format!("contract {index} key {problem}")));
        }
        let range = checked_range(item.range.start, item.range.end, line_count)
            .map_err(|e| invalid(format!("contract {index}: {e}")))?;
        output.contracts.push(Contract {
            kind: item.kind.into(),
            key: item.key,
            role: item.role.into(),
            range,
        });
    }
    for (index, item) in raw.edges.into_iter().enumerate() {
        if let Some(problem) = key_problem(&item.from_symbol) {
            return Err(invalid(format!("edge {index} from-symbol {problem}")));
        }
        if let Some(problem) = key_problem(&item.to_symbol) {
            return Err(invalid(format!("edge {index} to-symbol {problem}")));
        }
        let range = checked_range(item.range.start, item.range.end, line_count)
            .map_err(|e| invalid(format!("edge {index}: {e}")))?;
        output.edges.push(Edge {
            from: item.from_symbol,
            to: item.to_symbol,
            kind: item.kind.into(),
            evidence: item.evidence.into(),
            resolution: item.resolution.into(),
            range,
        });
    }
    Ok(output)
}

/// Converts the plugin's `analyze-error` into [`PluginError::Declined`].
pub(crate) fn decline(raw: &WireAnalyzeError, plugin: &str) -> PluginError {
    let (kind, message) = match raw {
        WireAnalyzeError::Unsupported(message) => (DeclineKind::Unsupported, message),
        WireAnalyzeError::Failed(message) => (DeclineKind::Failed, message),
    };
    PluginError::Declined {
        plugin: plugin.to_string(),
        kind,
        message: sanitize(message, MAX_DECLINE_MESSAGE_BYTES),
    }
}

/// Validates the plugin's `plugin-info` and checks it against the manifest.
pub(crate) fn validate_info(
    raw: WirePluginInfo,
    manifest: &Manifest,
) -> Result<PluginInfo, PluginError> {
    let plugin = manifest.name().as_str();
    let invalid = |reason: String| PluginError::InvalidMetadata {
        plugin: plugin.to_string(),
        reason,
    };
    let mismatch = |reason: String| PluginError::MetadataMismatch {
        plugin: plugin.to_string(),
        reason,
    };

    if raw.name != plugin {
        return Err(mismatch(format!(
            "name is `{}`, manifest says `{plugin}`",
            sanitize(&raw.name, MAX_INFO_TEXT_BYTES)
        )));
    }
    if raw.version != manifest.version() {
        return Err(mismatch(format!(
            "version is `{}`, manifest says `{}`",
            sanitize(&raw.version, MAX_INFO_TEXT_BYTES),
            manifest.version()
        )));
    }
    let languages = identifiers(raw.languages).map_err(|e| invalid(format!("languages {e}")))?;
    if languages.is_empty() {
        return Err(invalid("languages must not be empty".to_string()));
    }
    let frameworks = identifiers(raw.frameworks).map_err(|e| invalid(format!("frameworks {e}")))?;
    if raw.capabilities.len() > MAX_INFO_ENTRIES {
        return Err(invalid(format!(
            "capabilities has more than {MAX_INFO_ENTRIES} entries"
        )));
    }
    let mut capabilities = BTreeSet::new();
    for capability in raw.capabilities.into_iter().map(Capability::from) {
        if !manifest.requests(capability) {
            return Err(mismatch(format!(
                "plugin declares capability `{capability}`, which its manifest does not request"
            )));
        }
        capabilities.insert(capability);
    }
    Ok(PluginInfo {
        name: manifest.name().clone(),
        version: raw.version,
        languages,
        frameworks,
        capabilities,
    })
}

fn identifiers(list: Vec<String>) -> Result<BTreeSet<String>, String> {
    if list.len() > MAX_INFO_ENTRIES {
        return Err(format!("has more than {MAX_INFO_ENTRIES} entries"));
    }
    let mut out = BTreeSet::new();
    for id in list {
        if !is_identifier(&id) {
            return Err(format!(
                "entry `{}` must be 1-64 characters from [a-z0-9+#._-] starting with a letter or digit",
                sanitize(&id, MAX_INFO_TEXT_BYTES)
            ));
        }
        out.insert(id);
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use knowell_core::LineRange;

    use super::*;
    use crate::manifest::Sha256Digest;

    fn contract(key: &str, start: u32, end: u32) -> WireContract {
        WireContract {
            kind: WireContractKind::Table,
            key: key.to_string(),
            role: WireContractRole::Consumer,
            range: WireLineRange { start, end },
        }
    }

    fn edge(from: &str, to: &str) -> WireEdge {
        WireEdge {
            from_symbol: from.to_string(),
            to_symbol: to.to_string(),
            kind: WireEdgeKind::Reads,
            evidence: WireEvidence::Heuristic,
            resolution: WireResolution::Ambiguous,
            range: WireLineRange { start: 2, end: 3 },
        }
    }

    fn analysis(contracts: Vec<WireContract>, edges: Vec<WireEdge>) -> WireAnalysis {
        WireAnalysis { contracts, edges }
    }

    fn reason(result: Result<AnalyzerOutput, PluginError>) -> String {
        match result {
            Err(PluginError::InvalidOutput { reason, .. }) => reason,
            other => panic!("expected invalid output, got {other:?}"),
        }
    }

    #[test]
    fn converts_valid_output() {
        let raw = analysis(
            vec![contract("users", 1, 1)],
            vec![edge("repo.load", "users")],
        );
        let output = validate_analysis(raw, "p", 1024, 3).unwrap();
        assert_eq!(output.contracts[0].kind, ContractKind::Table);
        assert_eq!(output.contracts[0].role, ContractRole::Consumer);
        assert_eq!(output.contracts[0].range, LineRange::new(1, 1).unwrap());
        assert_eq!(output.edges[0].kind, EdgeKind::Reads);
        assert_eq!(output.edges[0].evidence, Evidence::Heuristic);
        assert_eq!(output.edges[0].resolution, Resolution::Ambiguous);
        assert_eq!(output.edges[0].range, LineRange::new(2, 3).unwrap());
    }

    #[test]
    fn enforces_the_output_budget_exactly() {
        // 32 + 5 bytes for the contract, 32 + 9 + 5 for the edge = 83.
        let make = || {
            analysis(
                vec![contract("users", 1, 1)],
                vec![edge("repo.load", "users")],
            )
        };
        assert!(validate_analysis(make(), "p", 83, 3).is_ok());
        assert!(matches!(
            validate_analysis(make(), "p", 82, 3),
            Err(PluginError::OutputTooLarge { limit: 82, .. })
        ));
        let empties = analysis((0..100).map(|_| contract("k", 1, 1)).collect(), Vec::new());
        assert!(matches!(
            validate_analysis(empties, "p", 3299, 1),
            Err(PluginError::OutputTooLarge { .. })
        ));
    }

    #[test]
    fn rejects_bad_keys_and_ranges() {
        let r = |c: WireContract| {
            reason(validate_analysis(
                analysis(vec![c], vec![]),
                "p",
                1 << 20,
                3,
            ))
        };
        assert!(r(contract("", 1, 1)).contains("contract 0 key is empty"));
        assert!(r(contract(" \t", 1, 1)).contains("blank"));
        assert!(r(contract("a\u{1b}b", 1, 1)).contains("control"));
        assert!(r(contract(&"k".repeat(513), 1, 1)).contains("longer than 512"));
        assert!(r(contract("k", 0, 1)).contains("start at 1"));
        assert!(r(contract("k", 3, 2)).contains("before start"));
        assert!(r(contract("k", 1, 4)).contains("outside the file"));
        let e = |e: WireEdge| {
            reason(validate_analysis(
                analysis(vec![], vec![e]),
                "p",
                1 << 20,
                3,
            ))
        };
        assert!(e(edge("", "x")).contains("edge 0 from-symbol"));
        assert!(e(edge("x", "\n")).contains("edge 0 to-symbol"));
        let mut far = edge("a", "b");
        far.range = WireLineRange { start: 9, end: 9 };
        assert!(e(far).contains("outside the file"));
    }

    #[test]
    fn declines_are_sanitised_and_truncated() {
        let raw = WireAnalyzeError::Failed(format!("bad\u{7}\n{}", "x".repeat(10_000)));
        match decline(&raw, "p") {
            PluginError::Declined { kind, message, .. } => {
                assert_eq!(kind, DeclineKind::Failed);
                assert!(message.starts_with("bad\u{FFFD} x"));
                assert!(message.len() <= MAX_DECLINE_MESSAGE_BYTES + '…'.len_utf8());
            }
            other => panic!("{other:?}"),
        }
        assert!(matches!(
            decline(&WireAnalyzeError::Unsupported(String::new()), "p"),
            PluginError::Declined {
                kind: DeclineKind::Unsupported,
                ..
            }
        ));
    }

    fn manifest(capabilities: &[Capability]) -> Manifest {
        Manifest::new(
            knowell_core::Name::new("demo").unwrap(),
            "1.0.0",
            Sha256Digest::of(b""),
            capabilities.iter().copied(),
        )
        .unwrap()
    }

    fn info() -> WirePluginInfo {
        WirePluginInfo {
            name: "demo".to_string(),
            version: "1.0.0".to_string(),
            languages: vec![
                "typescript".to_string(),
                "javascript".to_string(),
                "typescript".to_string(),
            ],
            frameworks: vec!["express".to_string()],
            capabilities: vec![],
        }
    }

    #[test]
    fn accepts_matching_metadata() {
        let validated = validate_info(info(), &manifest(&[])).unwrap();
        assert_eq!(validated.version(), "1.0.0");
        assert_eq!(
            validated.languages().iter().collect::<Vec<_>>(),
            ["javascript", "typescript"],
            "sorted and deduplicated"
        );
        let with_files = WirePluginInfo {
            capabilities: vec![WireCapability::ProjectFiles],
            ..info()
        };
        let validated = validate_info(with_files, &manifest(&[Capability::ProjectFiles])).unwrap();
        assert!(validated.capabilities().contains(&Capability::ProjectFiles));
    }

    #[test]
    fn rejects_bad_metadata() {
        let m = manifest(&[]);
        let mismatches = [
            WirePluginInfo {
                name: "other".to_string(),
                ..info()
            },
            WirePluginInfo {
                version: "1.0.1".to_string(),
                ..info()
            },
            WirePluginInfo {
                capabilities: vec![WireCapability::ProjectFiles],
                ..info()
            },
        ];
        for raw in mismatches {
            assert!(matches!(
                validate_info(raw, &m),
                Err(PluginError::MetadataMismatch { .. })
            ));
        }
        let invalid = [
            WirePluginInfo {
                languages: vec![],
                ..info()
            },
            WirePluginInfo {
                languages: vec!["Type Script".to_string()],
                ..info()
            },
            WirePluginInfo {
                languages: (0..65).map(|i| format!("l{i}")).collect(),
                ..info()
            },
            WirePluginInfo {
                frameworks: vec!["\u{1b}[31m".to_string()],
                ..info()
            },
            WirePluginInfo {
                capabilities: vec![WireCapability::ProjectFiles; 65],
                ..info()
            },
        ];
        for raw in invalid {
            assert!(matches!(
                validate_info(raw, &manifest(&[Capability::ProjectFiles])),
                Err(PluginError::InvalidMetadata { .. })
            ));
        }
    }

    #[test]
    fn hostcall_fuel_covers_the_output_limit() {
        assert_eq!(hostcall_fuel(1), 1024 * 1024);
        assert_eq!(hostcall_fuel(4 << 20), 8 << 20);
        assert_eq!(hostcall_fuel(u64::MAX), usize::MAX);
        // Host-side element sizes stay within twice the per-item accounting.
        assert!(std::mem::size_of::<WireContract>() as u64 <= 2 * ITEM_OVERHEAD_BYTES);
        assert!(std::mem::size_of::<WireEdge>() as u64 <= 2 * ITEM_OVERHEAD_BYTES);
    }
}
