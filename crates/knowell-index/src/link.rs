//! Cross-project contract linking as the T3 relation stage.
//!
//! [`LinkRelationStage`] runs the rule packs of `knowell-link` (HTTP
//! endpoints, topics, RPCs, tables, env names, i18n keys) over a project's
//! files and links the result with the other projects of its workspace. It
//! is the default relation stage of an [`crate::Indexer`].
//!
//! Inside the indexer (T3 of a build of generation `g`, before activation):
//!
//! 1. Nothing happens when the build changed no file: the rows of earlier
//!    generations stay valid.
//! 2. Every file of the project at `g` is extracted, from the redacted text
//!    the store holds. Excluded files never reached the store, so they are
//!    never read; the project's exclusion policy is applied again anyway.
//!    Pack activation (dependency manifests) and constant bindings are
//!    project-wide, so extraction is per project, not per changed file.
//! 3. The other projects of the workspace that this indexer serves are taken
//!    from an in-memory cache keyed by view and active generation, or
//!    extracted from their active generation's stored text on a cold cache.
//! 4. The link output of this project becomes edges and contract
//!    participations with origin `link:<path>`, sources mapped to the
//!    store's symbol ids (or the file when no definition matches). Only
//!    origins whose rows differ from what the generation already holds are
//!    replaced, so an unchanged file writes nothing.
//!
//! Cost: each file is parsed twice per build (once by `knowell-parse` in
//! T1, once by the packs' queries; `extract_project` takes text, not trees),
//! and the whole project is extracted on every build that changed it. The
//! file count is bounded by [`LinkRelationStage::with_max_files`]; above it
//! T3 is reported as failed with the reason (the generation still activates).
//! Edges and contracts of *other* projects are refreshed when those projects
//! are rebuilt; `infra` contracts have no store kind yet and are counted as
//! skipped.
//!
//! Through the plain [`RelationStage`] trait (without the store), the stage
//! extracts only the changed files it is handed and records file-level
//! sources; the indexer uses the store-backed path described above.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::sync::{Arc, Mutex, PoisonError};

use knowell_core::{ContentHash, LineRange, Name, RepoPath};
use knowell_link::{
    ExtractOptions, LinkOptions, LinkOutput, PackSet, ProjectExtractions, extract_project, link,
};
use knowell_parse::ParseLimits;
use knowell_secrets::ExclusionPolicy;
use knowell_store::graph::{NewContract, NewEdge, NodeRef};
use knowell_store::symbols::Definition;
use knowell_store::{
    ContractKind, ContractRole, EvidenceType, ProjectId, Resolution, SymbolId, ViewId, WorkspaceId,
};

use crate::analyze::split_symbol_key;
use crate::relate::{RelationError, RelationInput, RelationOutput, RelationStage};

/// Name (and origin prefix) of the link stage.
pub const LINK_STAGE_NAME: &str = "link";
/// Default most files of one project the link stage extracts.
pub const DEFAULT_MAX_LINK_FILES: usize = 20_000;
/// Evidence references kept per edge.
const MAX_EDGE_REFS: usize = 8;

/// Extractions of one project at one generation, cached for linking the
/// workspace's other projects.
#[derive(Debug, Clone)]
pub(crate) struct CachedExtractions {
    pub(crate) generation: i64,
    pub(crate) workspace: WorkspaceId,
    pub(crate) extractions: Arc<ProjectExtractions>,
}

/// The built-in T3 stage: contract extraction and cross-project linking with
/// `knowell-link` (see the module docs). Cheap to share; the cache of
/// per-project extractions is internal.
pub struct LinkRelationStage {
    packs: PackSet,
    options: LinkOptions,
    limits: ParseLimits,
    max_files: usize,
    cache: Mutex<BTreeMap<ViewId, CachedExtractions>>,
}

impl fmt::Debug for LinkRelationStage {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("LinkRelationStage")
            .field("packs", &self.packs.packs().len())
            .field("options", &self.options)
            .field("max_files", &self.max_files)
            .finish_non_exhaustive()
    }
}

impl LinkRelationStage {
    /// The stage with the bundled, validated rule packs.
    ///
    /// # Errors
    /// [`RelationError`] when a bundled pack fails validation (a bug).
    pub fn builtin() -> Result<Self, RelationError> {
        let packs = PackSet::builtin()
            .map_err(|e| RelationError(format!("loading the bundled rule packs: {e}")))?;
        Ok(Self::new(packs))
    }

    /// The stage with the given packs and default options.
    pub fn new(packs: PackSet) -> Self {
        Self {
            packs,
            options: LinkOptions::default(),
            limits: ParseLimits::default(),
            max_files: DEFAULT_MAX_LINK_FILES,
            cache: Mutex::new(BTreeMap::new()),
        }
    }

    /// Link options (gateway path prefixes, external contracts).
    #[must_use]
    pub fn with_link_options(mut self, options: LinkOptions) -> Self {
        self.options = options;
        self
    }

    /// Parse bounds of the packs' queries.
    #[must_use]
    pub fn with_parse_limits(mut self, limits: ParseLimits) -> Self {
        self.limits = limits;
        self
    }

    /// Most files of one project the stage extracts; a larger project's T3
    /// fails with a message naming the limit (nothing partial is linked).
    #[must_use]
    pub fn with_max_files(mut self, max_files: usize) -> Self {
        self.max_files = max_files;
        self
    }

    /// The configured file bound.
    pub fn max_files(&self) -> usize {
        self.max_files
    }

    /// The rule packs.
    pub fn packs(&self) -> &PackSet {
        &self.packs
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, BTreeMap<ViewId, CachedExtractions>> {
        self.cache.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Cached extractions of a view, if any.
    pub(crate) fn cached(&self, view: ViewId) -> Option<CachedExtractions> {
        self.lock().get(&view).cloned()
    }

    /// Remembers a view's extractions (replacing older ones).
    pub(crate) fn remember(&self, view: ViewId, entry: CachedExtractions) {
        let mut cache = self.lock();
        let newer = cache
            .get(&view)
            .is_none_or(|old| old.generation <= entry.generation);
        if newer {
            cache.insert(view, entry);
        }
    }

    /// Runs the active packs over one project's files. `files` hold redacted
    /// text; paths `policy` excludes are skipped without being read.
    pub(crate) fn extract(
        &self,
        project: &Name,
        files: &[(RepoPath, Arc<str>)],
        policy: &ExclusionPolicy,
    ) -> ProjectExtractions {
        let options = ExtractOptions {
            limits: self.limits,
            exclusion: policy.clone(),
            ..ExtractOptions::default()
        };
        let texts: BTreeMap<&RepoPath, &Arc<str>> = files.iter().map(|(p, t)| (p, t)).collect();
        let paths: Vec<RepoPath> = files.iter().map(|(p, _)| p.clone()).collect();
        extract_project(project, &paths, &self.packs, &options, &mut |path| {
            texts.get(path).map(|t| t.to_string())
        })
    }

    /// Links the given projects.
    pub(crate) fn link(
        &self,
        projects: &[ProjectExtractions],
    ) -> Result<LinkOutput, RelationError> {
        link(projects, &self.options).map_err(|e| RelationError(format!("linking: {e}")))
    }
}

impl RelationStage for LinkRelationStage {
    fn name(&self) -> &str {
        LINK_STAGE_NAME
    }

    /// Without the store: extracts the changed files it is given, links them
    /// with the cached extractions of the workspace's other projects and
    /// records file-level sources (symbol ids need the store). Origins cover
    /// the changed and removed paths.
    fn relate(&self, input: &RelationInput<'_>) -> Result<RelationOutput, RelationError> {
        let files: Vec<(RepoPath, Arc<str>)> = input
            .changed
            .iter()
            .map(|f| (f.path.clone(), Arc::clone(&f.text)))
            .collect();
        let hashes: BTreeMap<RepoPath, ContentHash> = input
            .changed
            .iter()
            .map(|f| (f.path.clone(), f.content_hash))
            .collect();
        let own = self.extract(input.project_name, &files, &ExclusionPolicy::builtin());
        let mut projects = vec![own.clone()];
        for (view, cached) in self.lock().iter() {
            if *view != input.view
                && cached.workspace == input.workspace
                && cached.extractions.project != *input.project_name
            {
                projects.push((*cached.extractions).clone());
            }
        }
        let output = self.link(&projects)?;
        let symbols = SymbolIndex::default();
        let cx = RowContext {
            project: input.project,
            project_name: input.project_name,
            workspace: input.workspace,
            hashes: &hashes,
            symbols: &symbols,
        };
        let rows = rows(&output, &own, &cx);
        let written: BTreeSet<String> = hashes.keys().map(origin).collect();
        let origins: BTreeSet<String> = written
            .iter()
            .cloned()
            .chain(input.removed.iter().map(origin))
            .collect();
        Ok(RelationOutput {
            origins: origins.into_iter().collect(),
            edges: rows
                .edges
                .into_iter()
                .filter(|e| written.contains(&e.origin))
                .collect(),
            contracts: rows
                .contracts
                .into_iter()
                .filter(|c| written.contains(&c.origin))
                .collect(),
        })
    }
}

/// Origin of the link rows of one file.
pub(crate) fn origin(path: &RepoPath) -> String {
    format!("{LINK_STAGE_NAME}:{path}")
}

/// Store symbol ids by (path, in-file qualified name).
#[derive(Debug, Clone, Default)]
pub(crate) struct SymbolIndex {
    by_key: BTreeMap<(RepoPath, String), Vec<(SymbolId, LineRange)>>,
}

impl SymbolIndex {
    pub(crate) fn from_definitions(definitions: &[Definition]) -> Self {
        let mut index = Self::default();
        for definition in definitions {
            if let Some((path, local)) = split_symbol_key(&definition.symbol.qualified_name) {
                index
                    .by_key
                    .entry((path, local.to_owned()))
                    .or_default()
                    .push((definition.symbol.id, definition.lines));
            }
        }
        for candidates in index.by_key.values_mut() {
            candidates.sort();
            candidates.dedup();
        }
        index
    }

    /// The symbol a link source names; with several candidates of that name
    /// (a type and its `impl` block), the one declared at `range`, else the
    /// one starting on the same line, else the first by id.
    pub(crate) fn find(
        &self,
        path: &RepoPath,
        local: &str,
        range: Option<LineRange>,
    ) -> Option<SymbolId> {
        let candidates = self.by_key.get(&(path.clone(), local.to_owned()))?;
        if let [(only, _)] = candidates.as_slice() {
            return Some(*only);
        }
        let exact = range.and_then(|r| candidates.iter().find(|(_, lines)| *lines == r));
        let same_start = range.and_then(|r| {
            candidates
                .iter()
                .find(|(_, lines)| lines.start() == r.start())
        });
        exact
            .or(same_start)
            .or(candidates.first())
            .map(|(id, _)| *id)
    }
}

/// What the row mapping needs to know about the project being built.
pub(crate) struct RowContext<'a> {
    pub(crate) project: ProjectId,
    pub(crate) project_name: &'a Name,
    pub(crate) workspace: WorkspaceId,
    /// Content hash of every file of the generation (the store's identity of
    /// the file version; the packs only see redacted text).
    pub(crate) hashes: &'a BTreeMap<RepoPath, ContentHash>,
    pub(crate) symbols: &'a SymbolIndex,
}

/// Store rows of one project's link output.
#[derive(Debug, Clone, Default, PartialEq)]
pub(crate) struct LinkRows {
    pub(crate) edges: Vec<NewEdge>,
    pub(crate) contracts: Vec<NewContract>,
    /// Edges and contracts that have no store representation (`infra`).
    pub(crate) skipped: usize,
}

fn evidence_type(e: knowell_graph::EvidenceType) -> EvidenceType {
    match e {
        knowell_graph::EvidenceType::SemanticResolved => EvidenceType::SemanticResolved,
        knowell_graph::EvidenceType::RuntimeObserved => EvidenceType::RuntimeObserved,
        knowell_graph::EvidenceType::ContractDerived => EvidenceType::ContractDerived,
        knowell_graph::EvidenceType::Syntactic => EvidenceType::Syntactic,
        knowell_graph::EvidenceType::Heuristic => EvidenceType::Heuristic,
        knowell_graph::EvidenceType::ModelSuggestion => EvidenceType::ModelSuggestion,
    }
}

fn resolution(r: knowell_graph::Resolution) -> Resolution {
    match r {
        knowell_graph::Resolution::Resolved => Resolution::Resolved,
        knowell_graph::Resolution::Ambiguous => Resolution::Ambiguous,
        knowell_graph::Resolution::Unresolved => Resolution::Unresolved,
    }
}

/// The store's contract kind; `None` for kinds the store has no value for.
fn contract_kind(kind: knowell_graph::ContractKind) -> Option<ContractKind> {
    Some(match kind {
        knowell_graph::ContractKind::Endpoint => ContractKind::Endpoint,
        knowell_graph::ContractKind::Topic => ContractKind::Topic,
        knowell_graph::ContractKind::Rpc => ContractKind::Rpc,
        knowell_graph::ContractKind::Table => ContractKind::Table,
        knowell_graph::ContractKind::EnvName => ContractKind::EnvName,
        knowell_graph::ContractKind::I18nKey => ContractKind::I18nKey,
        knowell_graph::ContractKind::Package => ContractKind::Package,
        knowell_graph::ContractKind::Infra => return None,
    })
}

fn lines_json(range: Option<LineRange>) -> serde_json::Value {
    match range {
        Some(r) => serde_json::json!([r.start(), r.end()]),
        None => serde_json::Value::Null,
    }
}

/// The store node and file of a link source node of this project.
fn source_ref(
    id: &knowell_graph::NodeId,
    node: Option<&knowell_graph::Node>,
    cx: &RowContext<'_>,
) -> Option<(NodeRef, RepoPath)> {
    let text = id.as_str();
    if let Some(rest) = text.strip_prefix(&format!("file:{}:", cx.project_name)) {
        let path = RepoPath::new(rest).ok()?;
        return Some((
            NodeRef::File {
                project: cx.project,
                path: path.clone(),
            },
            path,
        ));
    }
    let rest = text.strip_prefix(&format!("symbol:{}:", cx.project_name))?;
    let (path, local) = split_symbol_key(rest)?;
    let range = node.and_then(|n| n.source.as_ref()).and_then(|s| s.range);
    let from = match cx.symbols.find(&path, local, range) {
        Some(symbol) => NodeRef::Symbol(symbol),
        None => NodeRef::File {
            project: cx.project,
            path: path.clone(),
        },
    };
    Some((from, path))
}

/// Maps one project's link output and extractions to store rows, sorted.
pub(crate) fn rows(
    output: &LinkOutput,
    extractions: &ProjectExtractions,
    cx: &RowContext<'_>,
) -> LinkRows {
    let mut out = LinkRows::default();
    let sources = output.sources().get(cx.project_name);
    for record in output
        .edges()
        .get(cx.project_name)
        .into_iter()
        .flat_map(BTreeMap::values)
    {
        let Some((from, path)) =
            source_ref(&record.from, sources.and_then(|s| s.get(&record.from)), cx)
        else {
            out.skipped += 1;
            continue;
        };
        let to = output
            .contracts()
            .get(&record.to)
            .and_then(|node| match &node.kind {
                knowell_graph::NodeKind::Contract { kind, key } => {
                    contract_kind(*kind).map(|kind| NodeRef::Contract {
                        workspace: cx.workspace,
                        kind,
                        key: key.clone(),
                    })
                }
                _ => None,
            });
        let Some(to) = to else {
            out.skipped += 1;
            continue;
        };
        let first = record
            .edge
            .evidence_refs
            .iter()
            .find(|r| r.path == path)
            .or(record.edge.evidence_refs.first());
        let refs: Vec<serde_json::Value> = record
            .edge
            .evidence_refs
            .iter()
            .take(MAX_EDGE_REFS)
            .map(|r| {
                serde_json::json!({
                    "project": r.project.as_str(),
                    "path": r.path.as_str(),
                    "lines": lines_json(r.range),
                })
            })
            .collect();
        let evidence = serde_json::json!({
            "path": path.as_str(),
            "content_hash": cx.hashes.get(&path).map(ContentHash::to_string),
            "lines": lines_json(first.and_then(|r| r.range)),
            "refs": refs,
            "attrs": record.edge.attrs,
        });
        out.edges.push(NewEdge {
            from,
            to,
            kind: record.edge.kind.as_str().to_owned(),
            evidence_type: evidence_type(record.edge.evidence),
            resolution: resolution(record.edge.resolution),
            evidence,
            origin: origin(&path),
        });
    }
    for e in &extractions.extractions {
        let Some(kind) = contract_kind(e.kind) else {
            out.skipped += 1;
            continue;
        };
        if e.key.is_empty() {
            out.skipped += 1;
            continue;
        }
        let role = if e.role == knowell_link::Role::Definition || e.role.is_provider(e.kind) {
            ContractRole::Producer
        } else {
            ContractRole::Consumer
        };
        let symbol = e
            .symbol
            .as_ref()
            .and_then(|s| cx.symbols.find(&e.path, &s.qualified_name, Some(s.range)));
        let evidence = serde_json::json!({
            "path": e.path.as_str(),
            "content_hash": cx.hashes.get(&e.path).map(ContentHash::to_string),
            "lines": [e.range.start(), e.range.end()],
            "pack": e.pack,
            "rule": e.rule,
            "dynamic": e.dynamic,
            "attrs": e.attrs,
        });
        out.contracts.push(NewContract {
            kind,
            key: e.key.clone(),
            role,
            origin: origin(&e.path),
            symbol,
            evidence_type: evidence_type(e.evidence),
            evidence,
        });
    }
    out.edges.sort_by_key(edge_key);
    out.contracts.sort_by_key(contract_key);
    out
}

type EdgeKey = (
    String,
    NodeRef,
    NodeRef,
    String,
    EvidenceType,
    Resolution,
    String,
);

/// A total, deterministic order (and identity) of edge rows.
pub(crate) fn edge_key(e: &NewEdge) -> EdgeKey {
    (
        e.origin.clone(),
        e.from.clone(),
        e.to.clone(),
        e.kind.clone(),
        e.evidence_type,
        e.resolution,
        e.evidence.to_string(),
    )
}

type ContractKey = (
    String,
    ContractKind,
    String,
    ContractRole,
    Option<SymbolId>,
    EvidenceType,
    String,
);

/// A total, deterministic order (and identity) of contract rows.
pub(crate) fn contract_key(c: &NewContract) -> ContractKey {
    (
        c.origin.clone(),
        c.kind,
        c.key.clone(),
        c.role,
        c.symbol,
        c.evidence_type,
        c.evidence.to_string(),
    )
}

/// The origins (among `universe`) whose new rows differ from the stored
/// ones, as a relation output that replaces exactly those origins.
pub(crate) fn changed_origins(
    universe: &BTreeSet<String>,
    new: LinkRows,
    old_edges: &[NewEdge],
    old_contracts: &[NewContract],
) -> RelationOutput {
    let mut new_e: BTreeMap<&str, Vec<EdgeKey>> = BTreeMap::new();
    for e in &new.edges {
        new_e
            .entry(e.origin.as_str())
            .or_default()
            .push(edge_key(e));
    }
    let mut old_e: BTreeMap<&str, Vec<EdgeKey>> = BTreeMap::new();
    for e in old_edges {
        old_e
            .entry(e.origin.as_str())
            .or_default()
            .push(edge_key(e));
    }
    let mut new_c: BTreeMap<&str, Vec<ContractKey>> = BTreeMap::new();
    for c in &new.contracts {
        new_c
            .entry(c.origin.as_str())
            .or_default()
            .push(contract_key(c));
    }
    let mut old_c: BTreeMap<&str, Vec<ContractKey>> = BTreeMap::new();
    for c in old_contracts {
        old_c
            .entry(c.origin.as_str())
            .or_default()
            .push(contract_key(c));
    }
    for list in new_e.values_mut().chain(old_e.values_mut()) {
        list.sort();
    }
    for list in new_c.values_mut().chain(old_c.values_mut()) {
        list.sort();
    }
    let changed: BTreeSet<String> = universe
        .iter()
        .filter(|o| {
            let o = o.as_str();
            new_e.get(o) != old_e.get(o) || new_c.get(o) != old_c.get(o)
        })
        .cloned()
        .collect();
    RelationOutput {
        edges: new
            .edges
            .into_iter()
            .filter(|e| changed.contains(&e.origin))
            .collect(),
        contracts: new
            .contracts
            .into_iter()
            .filter(|c| changed.contains(&c.origin))
            .collect(),
        origins: changed.into_iter().collect(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::relate::validate_output;

    fn p(s: &str) -> RepoPath {
        RepoPath::new(s).unwrap()
    }

    fn name(s: &str) -> Name {
        Name::new(s).unwrap()
    }

    fn files(list: &[(&str, &str)]) -> Vec<(RepoPath, Arc<str>)> {
        list.iter().map(|(p_, t)| (p(p_), Arc::from(*t))).collect()
    }

    fn hashes(list: &[(RepoPath, Arc<str>)]) -> BTreeMap<RepoPath, ContentHash> {
        list.iter()
            .map(|(p_, t)| (p_.clone(), ContentHash::of(t.as_bytes())))
            .collect()
    }

    #[test]
    fn a_client_and_a_server_link_through_one_contract() {
        let stage = LinkRelationStage::builtin().unwrap();
        let web_files = files(&[(
            "src/api.ts",
            "export const load = () => fetch(\"/v1/plans\");\n",
        )]);
        let api_files = files(&[(
            "app.py",
            "from fastapi import FastAPI\napp = FastAPI()\n\n@app.get(\"/v1/plans\")\ndef plans():\n    return []\n",
        )]);
        let policy = ExclusionPolicy::builtin();
        let web = stage.extract(&name("web"), &web_files, &policy);
        let api = stage.extract(&name("api"), &api_files, &policy);
        let output = stage.link(&[web.clone(), api.clone()]).unwrap();
        let workspace = WorkspaceId(uuid::Uuid::nil());
        let hashes = hashes(&web_files);
        let cx = RowContext {
            project: ProjectId(uuid::Uuid::from_u128(1)),
            project_name: &name("web"),
            workspace,
            hashes: &hashes,
            symbols: &SymbolIndex::default(),
        };
        let rows = rows(&output, &web, &cx);
        let contract = NodeRef::Contract {
            workspace,
            kind: ContractKind::Endpoint,
            key: "GET /v1/plans".to_owned(),
        };
        let edge = rows
            .edges
            .iter()
            .find(|e| e.to == contract)
            .expect("the client consumes the endpoint");
        assert_eq!(edge.origin, "link:src/api.ts");
        assert_eq!(edge.kind, "consumes");
        assert_eq!(
            edge.evidence["content_hash"],
            serde_json::json!(hashes[&p("src/api.ts")].to_string())
        );
        assert!(
            rows.contracts
                .iter()
                .any(|c| c.key == "GET /v1/plans" && c.role == ContractRole::Consumer)
        );
        // Rows are valid relation output for the stage's origins.
        let universe: BTreeSet<String> = [origin(&p("src/api.ts"))].into();
        let output = changed_origins(&universe, rows.clone(), &[], &[]);
        assert_eq!(output.origins, vec!["link:src/api.ts".to_owned()]);
        validate_output(LINK_STAGE_NAME, &output).unwrap();
        // Rows identical to the stored ones change nothing.
        let same = changed_origins(&universe, rows.clone(), &rows.edges, &rows.contracts);
        assert!(same.origins.is_empty() && same.edges.is_empty());
        // A file whose rows disappeared is replaced with nothing.
        let gone = changed_origins(&universe, LinkRows::default(), &rows.edges, &rows.contracts);
        assert_eq!(gone.origins, vec!["link:src/api.ts".to_owned()]);
        assert!(gone.edges.is_empty() && gone.contracts.is_empty());
    }

    #[test]
    fn symbol_sources_map_to_store_ids_by_name_and_lines() {
        let id = |n: u128| SymbolId(uuid::Uuid::from_u128(n));
        let definition = |n: u128, name: &str, start: u32| Definition {
            symbol: knowell_store::symbols::Symbol {
                id: id(n),
                project: ProjectId(uuid::Uuid::nil()),
                qualified_name: name.to_owned(),
                kind: "struct".to_owned(),
                created_at: time::OffsetDateTime::UNIX_EPOCH,
                updated_at: time::OffsetDateTime::UNIX_EPOCH,
            },
            path: p("src/a.rs"),
            content_hash: ContentHash::of(b"a"),
            lines: LineRange::new(start, start + 2).unwrap(),
        };
        let index = SymbolIndex::from_definitions(&[
            definition(1, "src/a.rs#Plan", 1),
            definition(2, "src/a.rs#Plan", 10),
            definition(3, "src/a.rs#load", 20),
        ]);
        assert_eq!(index.find(&p("src/a.rs"), "load", None), Some(id(3)));
        assert_eq!(
            index.find(
                &p("src/a.rs"),
                "Plan",
                Some(LineRange::new(10, 12).unwrap())
            ),
            Some(id(2))
        );
        assert_eq!(
            index.find(
                &p("src/a.rs"),
                "Plan",
                Some(LineRange::new(10, 30).unwrap())
            ),
            Some(id(2))
        );
        assert_eq!(index.find(&p("src/a.rs"), "Plan", None), Some(id(1)));
        assert_eq!(index.find(&p("src/b.rs"), "Plan", None), None);
    }

    #[test]
    fn the_trait_path_extracts_changed_files_only() {
        let stage = LinkRelationStage::builtin().unwrap();
        let changed = vec![crate::relate::RelationFile {
            path: p("src/api.ts"),
            content_hash: ContentHash::of(b"x"),
            text: Arc::from("export const load = () => fetch(\"/v1/plans\");\n"),
            parsed: Arc::new(knowell_parse::parse(
                &p("src/api.ts"),
                "export const load = () => fetch(\"/v1/plans\");\n",
            )),
        }];
        let project_name = name("web");
        let removed = [p("src/old.ts")];
        let input = RelationInput {
            workspace: WorkspaceId(uuid::Uuid::nil()),
            project: ProjectId(uuid::Uuid::nil()),
            project_name: &project_name,
            view: ViewId(uuid::Uuid::nil()),
            generation: 1,
            changed: &changed,
            removed: &removed,
        };
        let output = stage.relate(&input).unwrap();
        validate_output(stage.name(), &output).unwrap();
        assert_eq!(
            output.origins,
            vec!["link:src/api.ts".to_owned(), "link:src/old.ts".to_owned()]
        );
        assert!(
            output
                .edges
                .iter()
                .all(|e| matches!(e.from, NodeRef::File { .. }))
        );
        assert!(!output.contracts.is_empty());
    }
}
