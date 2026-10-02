//! Cross-project contract linking: declarative rule packs extract
//! endpoints, events, RPCs, tables, env names and i18n keys; the linker
//! matches producers and consumers across projects with explicit evidence;
//! checks report drift and gaps (SARIF for CI).
//!
//! Pipeline:
//!
//! 1. [`PackSet::builtin`] (or [`Pack::load_dir`]) loads validated rule packs:
//!    `pack.toml` + tree-sitter queries + Rust structured extractors for
//!    document formats (OpenAPI, AsyncAPI, JSON Schema, proto, Compose,
//!    Kubernetes, locale files, Prisma).
//! 2. [`extract_project`] runs the active packs over one project's files and
//!    returns [`ProjectExtractions`]: normalised contract keys with roles,
//!    symbols, line ranges and attributes. Sensitive paths are never read,
//!    content is redacted before analysis, and environment variables are
//!    recorded by name only.
//! 3. [`link`] connects the extractions of every project of a workspace view
//!    into contract nodes and evidence-carrying edges ([`LinkOutput`]), which
//!    become [`knowell_graph::GraphDelta`]s via [`LinkOutput::deltas`].
//! 4. [`check`] runs the knowell-graph insights plus link-specific checks and
//!    returns [`Finding`]s; [`to_sarif`] renders them as SARIF 2.1.0.
//!
//! ```
//! use knowell_core::{Name, RepoPath};
//! use knowell_link::{
//!     CheckOptions, ExtractOptions, LinkOptions, PackSet, check, extract_project, link,
//! };
//!
//! let packs = PackSet::builtin()?;
//! let web = Name::new("web").unwrap();
//! let api = Name::new("api").unwrap();
//! let files = |path: &str| vec![RepoPath::new(path).unwrap()];
//! let client = extract_project(&web, &files("src/api.ts"), &packs, &ExtractOptions::default(),
//!     &mut |_| Some("export const load = () => fetch(\"/v1/plans\");\n".to_owned()));
//! let server = extract_project(&api, &files("app.py"), &packs, &ExtractOptions::default(),
//!     &mut |_| Some("from fastapi import FastAPI\napp = FastAPI()\n\n@app.get(\"/v1/plans\")\ndef plans():\n    return []\n".to_owned()));
//! let linked = link(&[client.clone(), server.clone()], &LinkOptions::default())?;
//! let graph = linked.graph()?;
//! assert!(graph.node(&knowell_graph::NodeId::contract(
//!     knowell_graph::ContractKind::Endpoint, "GET /v1/plans")).is_some());
//! let findings = check(&[client, server], &linked, &CheckOptions::default())?;
//! assert!(findings.iter().all(|f| f.code != "graph.endpoint_without_client"));
//! # Ok::<(), knowell_link::LinkError>(())
//! ```

mod check;
mod error;
mod extract;
mod link;
mod model;
mod normalize;
mod pack;
mod sarif;
mod structured;

pub use check::{CHECK_RULES, CheckOptions, CheckRule, Finding, Location, Severity, check};
pub use error::LinkError;
pub use extract::{ExtractOptions, PackRun, extract_project, run_pack_on_file};
pub use link::{
    ATTR_MATCH, ATTR_RULES, EntityMapping, LinkOptions, LinkOutput, TableSchema, TableVersion, link,
};
pub use model::{
    ATTR_COLUMN, ATTR_COLUMNS, ATTR_ENTITY, ATTR_FIELDS, ATTR_GLOB, ATTR_HOST, ATTR_NEW_NAME,
    ATTR_OP, ATTR_OPERATION, ATTR_UNRESOLVED_KEY, ATTR_VERSION, Extraction, ProjectExtractions,
    Role, Shape, SkippedFile, SymbolRef, parse_contract_kind,
};
pub use normalize::normalize_key;
pub use pack::{Pack, PackSet};
pub use sarif::{FINGERPRINT_KEY, fingerprint, to_sarif};

/// Version of the extraction and link output. Bump it when the same input
/// can produce different extractions, keys or edges, so stored link results
/// keyed on it are recomputed.
pub const LINK_FORMAT_VERSION: u32 = 1;
