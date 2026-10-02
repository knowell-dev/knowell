//! The linker: turns the extractions of every project of a workspace view
//! into contract nodes and evidence-carrying edges.
//!
//! Evidence (how an edge was established) and resolution (whether its target
//! is known) are decided separately:
//!
//! | Situation | Evidence | Resolution |
//! |---|---|---|
//! | definition document (OpenAPI, AsyncAPI, proto, JSON Schema, migration) | `ContractDerived` | `Resolved` |
//! | locale entry, env / service declaration | `Syntactic` | `Resolved` |
//! | literal code key equal to a contract backed by a definition document | `ContractDerived` | `Resolved` |
//! | literal code key, no definition document | rule evidence (`Syntactic`) | `Resolved` |
//! | heuristic rule, dynamic key, or pattern / prefix / suffix match | `Heuristic` | `Resolved` (one candidate) or `Ambiguous` (several) |
//! | dynamic key without candidates, fully dynamic key | rule evidence | `Unresolved` (placeholder node, `unresolved = "true"`) |
//!
//! Unresolved uses are never dropped: they point at a placeholder contract
//! node so they stay visible in traversals and checks.

use std::collections::{BTreeMap, BTreeSet};

use knowell_core::{Name, RepoPath};
use knowell_graph::{
    ATTR_EXTERNAL, ATTR_LOCALE, ATTR_SCHEMA_HASH, ATTR_UNRESOLVED, CodeGraph, ContractKind, Edge,
    EdgeKey, EdgeKind, EdgeRecord, EvidenceRef, EvidenceType, GraphDelta, Node, NodeId, Resolution,
};
use serde::{Deserialize, Serialize};

use crate::error::LinkError;
use crate::model::{
    ATTR_COLUMN, ATTR_COLUMNS, ATTR_ENTITY, ATTR_FIELDS, ATTR_GLOB, ATTR_NEW_NAME, ATTR_OP,
    ATTR_VERSION, Extraction, ProjectExtractions, Role, SymbolRef,
};
use crate::normalize::{Segment, endpoint_parts, glob_match};
use crate::structured::hash_hex;

/// Edge attribute: the `pack@version/rule` ids that produced the edge.
pub const ATTR_RULES: &str = "rules";
/// Edge attribute: how the key was matched (`exact`, `pattern`, `prefix`,
/// `suffix`, `glob`, `none`).
pub const ATTR_MATCH: &str = "match";

/// Options of [`link`].
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LinkOptions {
    /// Path prefixes a client may add in front of a server route (gateway
    /// base paths such as `/api`). Tried, heuristically, when a client path
    /// has no exact match.
    pub path_prefixes: Vec<String>,
    /// Contracts used from outside the indexed workspace; their nodes carry
    /// `external = "true"` and graph insights skip them.
    pub external: BTreeSet<(ContractKind, String)>,
}

/// One version of a table: its columns after a migration file.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TableVersion {
    /// Project holding the migration.
    pub project: Name,
    /// Migration file.
    pub path: RepoPath,
    /// Columns after the migration (lower case), sorted.
    pub columns: BTreeSet<String>,
    /// Hash of the column set (hex).
    pub hash: String,
}

/// The history of a table built from migrations, in file order.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TableSchema {
    /// Table key.
    pub name: String,
    /// Versions, oldest first; the last is current.
    pub versions: Vec<TableVersion>,
    /// Columns added by `ALTER TABLE ... ADD COLUMN` after the table was
    /// created, with the migration that added them.
    pub added: BTreeMap<String, EvidenceRef>,
    /// `true` when the last migration touching the table dropped it.
    pub dropped: bool,
}

impl TableSchema {
    /// Current columns (empty when dropped).
    pub fn columns(&self) -> BTreeSet<String> {
        self.versions
            .last()
            .map(|v| v.columns.clone())
            .unwrap_or_default()
    }

    /// Hash of the current column set.
    pub fn hash(&self) -> Option<&str> {
        self.versions.last().map(|v| v.hash.as_str())
    }

    /// The newest version an entity mapping `columns` was built against:
    /// all its columns exist in that version and every column added (by an
    /// `ALTER`) up to that version is mapped. `None` when no version fits.
    pub fn built_against(&self, columns: &BTreeSet<String>) -> Option<usize> {
        for k in (0..self.versions.len()).rev() {
            let version = self.versions.get(k)?;
            if !columns.is_subset(&version.columns) {
                continue;
            }
            let mut added = BTreeSet::new();
            for j in 1..=k {
                if let (Some(previous), Some(current)) =
                    (self.versions.get(j - 1), self.versions.get(j))
                {
                    added.extend(current.columns.difference(&previous.columns).cloned());
                }
            }
            if added
                .iter()
                .filter(|c| version.columns.contains(*c))
                .all(|c| columns.contains(c))
            {
                return Some(k);
            }
        }
        None
    }
}

/// A class / struct / model mapping a table, with the columns it maps.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EntityMapping {
    /// Project of the entity.
    pub project: Name,
    /// File of the entity.
    pub path: RepoPath,
    /// The entity symbol, when known.
    pub symbol: Option<SymbolRef>,
    /// Table key.
    pub table: String,
    /// Mapped columns (lower case).
    pub columns: BTreeSet<String>,
    /// Evidence of the mapping (declaration site).
    pub evidence: EvidenceRef,
}

/// The linker's output: contract nodes, per-project source nodes and edges,
/// table histories and entity mappings.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LinkOutput {
    contracts: BTreeMap<NodeId, Node>,
    sources: BTreeMap<Name, BTreeMap<NodeId, Node>>,
    edges: BTreeMap<Name, BTreeMap<EdgeKey, EdgeRecord>>,
    tables: BTreeMap<String, TableSchema>,
    entities: Vec<EntityMapping>,
}

impl LinkOutput {
    /// Contract and placeholder nodes, by id.
    pub fn contracts(&self) -> &BTreeMap<NodeId, Node> {
        &self.contracts
    }

    /// Every edge, by owning project (the project of its source node).
    pub fn edges(&self) -> &BTreeMap<Name, BTreeMap<EdgeKey, EdgeRecord>> {
        &self.edges
    }

    /// File and symbol nodes the edges start from, by project.
    pub fn sources(&self) -> &BTreeMap<Name, BTreeMap<NodeId, Node>> {
        &self.sources
    }

    /// Table histories from migrations, by table key.
    pub fn tables(&self) -> &BTreeMap<String, TableSchema> {
        &self.tables
    }

    /// ORM entity mappings, sorted by project, path and symbol.
    pub fn entities(&self) -> &[EntityMapping] {
        &self.entities
    }

    /// One delta per project at the given view generations. Source nodes
    /// (files, symbols) are only added when `existing` lacks them, so nodes
    /// written by the indexer keep their attributes; contract nodes are
    /// always written (they are fully determined by the link).
    ///
    /// # Errors
    /// [`LinkError::MissingGeneration`] when a project has no generation.
    pub fn deltas(
        &self,
        generations: &BTreeMap<Name, u64>,
        existing: Option<&CodeGraph>,
    ) -> Result<Vec<GraphDelta>, LinkError> {
        let projects: BTreeSet<&Name> = self.sources.keys().chain(self.edges.keys()).collect();
        let mut out = Vec::new();
        for project in projects {
            let generation = *generations
                .get(project)
                .ok_or_else(|| LinkError::MissingGeneration(project.clone()))?;
            let mut delta = GraphDelta::new(project.clone(), generation);
            if let Some(sources) = self.sources.get(project) {
                for node in sources.values() {
                    if existing.is_none_or(|g| g.node(&node.id).is_none()) {
                        delta = delta.add_node(node.clone());
                    }
                }
            }
            let mut contracts: BTreeSet<&NodeId> = BTreeSet::new();
            if let Some(edges) = self.edges.get(project) {
                for record in edges.values() {
                    contracts.insert(&record.to);
                }
            }
            for id in contracts {
                if let Some(node) = self.contracts.get(id) {
                    delta = delta.add_node(node.clone());
                }
            }
            if let Some(edges) = self.edges.get(project) {
                for record in edges.values() {
                    delta = delta.add_edge(&record.from, &record.to, record.edge.clone());
                }
            }
            out.push(delta);
        }
        Ok(out)
    }

    /// Like [`LinkOutput::deltas`], for replacing an earlier link of the
    /// same workspace that `graph` already holds: each delta also removes the
    /// edges `previous` produced for that project that this output no longer
    /// has (edges no longer in `graph` are skipped). Projects that dropped
    /// out entirely get a removal-only delta. Contract nodes left without
    /// edges are kept; they carry no project and other views may use them.
    ///
    /// # Errors
    /// [`LinkError::MissingGeneration`] when a project has no generation.
    pub fn replacing_deltas(
        &self,
        previous: &LinkOutput,
        generations: &BTreeMap<Name, u64>,
        graph: &CodeGraph,
    ) -> Result<Vec<GraphDelta>, LinkError> {
        let mut deltas: BTreeMap<Name, GraphDelta> = self
            .deltas(generations, Some(graph))?
            .into_iter()
            .map(|d| (d.project.clone(), d))
            .collect();
        for (project, old_edges) in &previous.edges {
            let current = self.edges.get(project);
            let stale: Vec<EdgeKey> = old_edges
                .keys()
                .filter(|key| current.is_none_or(|edges| !edges.contains_key(*key)))
                .filter(|key| graph.edge(key).is_some())
                .cloned()
                .collect();
            if stale.is_empty() {
                continue;
            }
            if !deltas.contains_key(project) {
                let generation = *generations
                    .get(project)
                    .ok_or_else(|| LinkError::MissingGeneration(project.clone()))?;
                deltas.insert(
                    project.clone(),
                    GraphDelta::new(project.clone(), generation),
                );
            }
            if let Some(delta) = deltas.get_mut(project) {
                delta.removed_edges.extend(stale);
            }
        }
        Ok(deltas.into_values().collect())
    }

    /// A fresh graph holding the link output (generation 1 for every
    /// project).
    ///
    /// # Errors
    /// [`LinkError::Graph`] when a delta is rejected (a bug).
    pub fn graph(&self) -> Result<CodeGraph, LinkError> {
        let generations: BTreeMap<Name, u64> = self
            .sources
            .keys()
            .chain(self.edges.keys())
            .map(|p| (p.clone(), 1))
            .collect();
        let mut graph = CodeGraph::new();
        for delta in self.deltas(&generations, None)? {
            graph.apply(delta)?;
        }
        Ok(graph)
    }
}

fn evidence_ref(e: &Extraction) -> EvidenceRef {
    EvidenceRef {
        project: e.project.clone(),
        path: e.path.clone(),
        range: Some(e.range),
        content_hash: e.content_hash,
    }
}

/// How a key was matched.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum MatchKind {
    Exact,
    Pattern,
    Prefix,
    Suffix,
    Glob,
    None,
}

impl MatchKind {
    fn as_str(self) -> &'static str {
        match self {
            Self::Exact => "exact",
            Self::Pattern => "pattern",
            Self::Prefix => "prefix",
            Self::Suffix => "suffix",
            Self::Glob => "glob",
            Self::None => "none",
        }
    }
}

/// Keys that exist independently of a consumer: definitions and providers.
#[derive(Default)]
struct KnownKeys {
    by_kind: BTreeMap<ContractKind, BTreeSet<String>>,
    defined_by_document: BTreeSet<(ContractKind, String)>,
}

impl KnownKeys {
    fn keys(&self, kind: ContractKind) -> impl Iterator<Item = &String> {
        self.by_kind.get(&kind).into_iter().flatten()
    }

    fn contains(&self, kind: ContractKind, key: &str) -> bool {
        self.by_kind.get(&kind).is_some_and(|k| k.contains(key))
    }
}

/// Segment-wise endpoint match score (`None`: no match). Higher is more
/// specific: literal-literal pairs count, an exact method counts.
fn endpoint_score(consumer: &str, provider: &str) -> Option<usize> {
    let (cm, cs) = endpoint_parts(consumer);
    let (pm, ps) = endpoint_parts(provider);
    let method_exact = cm == pm;
    if !(method_exact || cm == "*" || pm == "*") || cs.len() != ps.len() {
        return None;
    }
    let mut score = usize::from(method_exact);
    for (c, p) in cs.iter().zip(ps.iter()) {
        match (c, p) {
            (Segment::Literal(a), Segment::Literal(b)) => {
                if a != b {
                    return None;
                }
                score += 2;
            }
            (Segment::Literal(_) | Segment::Param | Segment::Glob(_), Segment::Param) => {}
            (Segment::Param, Segment::Literal(_) | Segment::Glob(_)) => {}
            (Segment::Glob(pattern), Segment::Literal(b)) => {
                if !glob_match(pattern, b) {
                    return None;
                }
                score += 1;
            }
            (Segment::Literal(a), Segment::Glob(pattern)) => {
                if !glob_match(pattern, a) {
                    return None;
                }
                score += 1;
            }
            (Segment::Glob(a), Segment::Glob(b)) => {
                if a != b {
                    return None;
                }
            }
        }
    }
    Some(score)
}

fn best_endpoints<'k>(key: &str, known: impl Iterator<Item = &'k String>) -> Vec<String> {
    let mut best: Vec<(usize, String)> = Vec::new();
    for candidate in known {
        if let Some(score) = endpoint_score(key, candidate) {
            best.push((score, candidate.clone()));
        }
    }
    let Some(top) = best.iter().map(|(s, _)| *s).max() else {
        return Vec::new();
    };
    let mut out: Vec<String> = best
        .into_iter()
        .filter(|(s, _)| *s == top)
        .map(|(_, k)| k)
        .collect();
    out.sort();
    out
}

fn rpc_parts(key: &str) -> (String, String) {
    let (service, method) = key.rsplit_once('/').unwrap_or(("", key));
    let short = service.rsplit('.').next().unwrap_or(service);
    (short.to_owned(), method.to_owned())
}

/// Candidate contract keys for a non-definition extraction.
fn resolve_targets(
    e: &Extraction,
    known: &KnownKeys,
    options: &LinkOptions,
) -> (Vec<String>, MatchKind) {
    if known.contains(e.kind, &e.key) {
        return (vec![e.key.clone()], MatchKind::Exact);
    }
    match e.kind {
        ContractKind::Endpoint => {
            let found = best_endpoints(&e.key, known.keys(ContractKind::Endpoint));
            if !found.is_empty() {
                return (found, MatchKind::Pattern);
            }
            let (method, path) = e.key.split_once(' ').unwrap_or(("*", e.key.as_str()));
            for prefix in &options.path_prefixes {
                let prefix = prefix.trim_end_matches('/');
                if prefix.is_empty() {
                    continue;
                }
                if let Some(rest) = path.strip_prefix(prefix)
                    && rest.starts_with('/')
                {
                    let stripped = format!("{method} {rest}");
                    if known.contains(ContractKind::Endpoint, &stripped) {
                        return (vec![stripped], MatchKind::Prefix);
                    }
                    let found = best_endpoints(&stripped, known.keys(ContractKind::Endpoint));
                    if !found.is_empty() {
                        return (found, MatchKind::Prefix);
                    }
                }
            }
            (Vec::new(), MatchKind::None)
        }
        ContractKind::Rpc => {
            let (service, method) = rpc_parts(&e.key);
            let mut found: Vec<String> = known
                .keys(ContractKind::Rpc)
                .filter(|candidate| {
                    let (s, m) = rpc_parts(candidate);
                    m.eq_ignore_ascii_case(&method)
                        && (service == "{}" || service.is_empty() || s == service)
                })
                .cloned()
                .collect();
            found.sort();
            if found.is_empty() {
                (Vec::new(), MatchKind::None)
            } else {
                (found, MatchKind::Suffix)
            }
        }
        _ if e.key.contains("{}") => {
            let mut found: Vec<String> = known
                .keys(e.kind)
                .filter(|candidate| !candidate.contains("{}") && glob_match(&e.key, candidate))
                .cloned()
                .collect();
            found.sort();
            if found.is_empty() {
                (Vec::new(), MatchKind::None)
            } else {
                (found, MatchKind::Glob)
            }
        }
        _ => (Vec::new(), MatchKind::None),
    }
}

/// Builds table histories from table definition extractions (migrations).
fn build_tables(definitions: &[&Extraction]) -> BTreeMap<String, TableSchema> {
    let mut ordered: Vec<&Extraction> = definitions.to_vec();
    ordered.sort_by(|a, b| {
        (
            a.project.as_str(),
            a.path.as_str(),
            a.range.start(),
            &a.key,
            &a.attrs,
        )
            .cmp(&(
                b.project.as_str(),
                b.path.as_str(),
                b.range.start(),
                &b.key,
                &b.attrs,
            ))
    });
    let mut tables: BTreeMap<String, TableSchema> = BTreeMap::new();
    let mut current: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    let mut created_in_file: BTreeSet<String> = BTreeSet::new();
    let mut touched_in_file: BTreeSet<String> = BTreeSet::new();
    let mut file: Option<(Name, RepoPath)> = None;

    let flush = |file: &Option<(Name, RepoPath)>,
                 touched: &mut BTreeSet<String>,
                 current: &BTreeMap<String, BTreeSet<String>>,
                 tables: &mut BTreeMap<String, TableSchema>| {
        let Some((project, path)) = file else {
            touched.clear();
            return;
        };
        for table in std::mem::take(touched) {
            let columns = current.get(&table).cloned().unwrap_or_default();
            let joined: Vec<&str> = columns.iter().map(String::as_str).collect();
            let hash = hash_hex("table-columns/v1", &[&table, &joined.join(",")]);
            let schema = tables.entry(table.clone()).or_insert_with(|| TableSchema {
                name: table.clone(),
                versions: Vec::new(),
                added: BTreeMap::new(),
                dropped: false,
            });
            schema.versions.push(TableVersion {
                project: project.clone(),
                path: path.clone(),
                columns,
                hash,
            });
        }
    };

    for e in ordered {
        let here = (e.project.clone(), e.path.clone());
        if file.as_ref() != Some(&here) {
            flush(&file, &mut touched_in_file, &current, &mut tables);
            created_in_file.clear();
            file = Some(here);
        }
        let table = e.key.clone();
        let column = e.attr(ATTR_COLUMN).map(str::to_lowercase);
        match e.attr(ATTR_OP).unwrap_or("create") {
            "create" => {
                if created_in_file.insert(table.clone()) {
                    current.insert(table.clone(), BTreeSet::new());
                    if let Some(schema) = tables.get_mut(&table) {
                        schema.dropped = false;
                        schema.added.clear();
                    }
                }
                if let Some(column) = column {
                    current.entry(table.clone()).or_default().insert(column);
                }
            }
            "add_column" => {
                if let Some(column) = column {
                    current
                        .entry(table.clone())
                        .or_default()
                        .insert(column.clone());
                    let schema = tables.entry(table.clone()).or_insert_with(|| TableSchema {
                        name: table.clone(),
                        versions: Vec::new(),
                        added: BTreeMap::new(),
                        dropped: false,
                    });
                    schema.added.insert(column, evidence_ref(e));
                }
            }
            "drop_column" => {
                if let Some(column) = column {
                    if let Some(columns) = current.get_mut(&table) {
                        columns.remove(&column);
                    }
                    if let Some(schema) = tables.get_mut(&table) {
                        schema.added.remove(&column);
                    }
                }
            }
            "rename_column" => {
                if let (Some(old), Some(new)) =
                    (column, e.attr(ATTR_NEW_NAME).map(str::to_lowercase))
                {
                    let columns = current.entry(table.clone()).or_default();
                    if columns.remove(&old) {
                        columns.insert(new.clone());
                    }
                    if let Some(schema) = tables.get_mut(&table)
                        && let Some(evidence) = schema.added.remove(&old)
                    {
                        schema.added.insert(new, evidence);
                    }
                }
            }
            "drop_table" => {
                current.insert(table.clone(), BTreeSet::new());
                if let Some(schema) = tables.get_mut(&table) {
                    schema.dropped = true;
                    schema.added.clear();
                }
            }
            "rename_table" => {
                if let Some(new) = e.attr(ATTR_NEW_NAME) {
                    let new = new.to_lowercase();
                    let columns = current
                        .insert(table.clone(), BTreeSet::new())
                        .unwrap_or_default();
                    current.insert(new.clone(), columns);
                    if let Some(schema) = tables.get_mut(&table) {
                        schema.dropped = true;
                        let added = std::mem::take(&mut schema.added);
                        let entry = tables.entry(new.clone()).or_insert_with(|| TableSchema {
                            name: new.clone(),
                            versions: Vec::new(),
                            added: BTreeMap::new(),
                            dropped: false,
                        });
                        entry.added.extend(added);
                    }
                    touched_in_file.insert(new);
                }
            }
            _ => {}
        }
        touched_in_file.insert(table);
    }
    flush(&file, &mut touched_in_file, &current, &mut tables);
    tables
}

/// ORM entity mappings: table reads that carry an `entity` or `column`
/// attribute, grouped by project, file, symbol and table.
fn build_entities(extractions: &[&Extraction]) -> Vec<EntityMapping> {
    let mut groups: BTreeMap<(Name, RepoPath, Option<String>, String), EntityMapping> =
        BTreeMap::new();
    for e in extractions {
        if e.kind != ContractKind::Table
            || !matches!(
                e.role,
                Role::Reads | Role::Consumer | Role::Writes | Role::Producer
            )
            || (e.attr(ATTR_COLUMN).is_none() && e.attr(ATTR_ENTITY).is_none())
        {
            continue;
        }
        let key = (
            e.project.clone(),
            e.path.clone(),
            e.symbol.as_ref().map(|s| s.qualified_name.clone()),
            e.key.clone(),
        );
        let entry = groups.entry(key).or_insert_with(|| EntityMapping {
            project: e.project.clone(),
            path: e.path.clone(),
            symbol: e.symbol.clone(),
            table: e.key.clone(),
            columns: BTreeSet::new(),
            evidence: EvidenceRef {
                project: e.project.clone(),
                path: e.path.clone(),
                range: Some(e.symbol.as_ref().map_or(e.range, |s| s.range)),
                content_hash: e.content_hash,
            },
        });
        if let Some(column) = e.attr(ATTR_COLUMN) {
            entry.columns.insert(column.to_lowercase());
        }
    }
    groups.into_values().collect()
}

/// The source node of an extraction: the symbol, or the file for
/// definitions and file-level code.
fn source_node(e: &Extraction) -> Node {
    match (&e.symbol, e.role) {
        (Some(symbol), role) if role != Role::Definition => {
            let name = symbol
                .qualified_name
                .rsplit(['.', ' '])
                .next()
                .unwrap_or(&symbol.qualified_name)
                .to_owned();
            Node::symbol(&e.project, &symbol.graph_key(&e.path), &name).with_source(EvidenceRef {
                project: e.project.clone(),
                path: e.path.clone(),
                range: Some(symbol.range),
                content_hash: e.content_hash,
            })
        }
        _ => {
            let mut node = Node::file(&e.project, &e.path);
            if e.role == Role::Definition
                && e.kind == ContractKind::I18nKey
                && let Some(locale) = e.attr(ATTR_LOCALE)
            {
                node = node.with_attr(ATTR_LOCALE, locale);
            }
            node
        }
    }
}

fn numeric_version(e: &Extraction) -> u64 {
    e.attr(ATTR_VERSION)
        .and_then(|v| v.parse::<u64>().ok())
        .unwrap_or(0)
}

/// Copies the extractions with code-side RPC keys (`Service/Method`, no
/// package) rewritten to the one proto definition they name
/// (`pkg.Service/Method`), so clients, servers and the definition share a
/// node. The flags mark rewritten keys: such matches are heuristic.
fn canonical_rpc_keys(projects: &[ProjectExtractions]) -> (Vec<Extraction>, Vec<bool>) {
    let definitions: BTreeSet<String> = projects
        .iter()
        .flat_map(|p| p.extractions.iter())
        .filter(|e| e.kind == ContractKind::Rpc && e.role == Role::Definition)
        .map(|e| e.key.clone())
        .collect();
    let mut owned = Vec::new();
    let mut flags = Vec::new();
    for e in projects.iter().flat_map(|p| p.extractions.iter()) {
        let mut copy = e.clone();
        let mut rewritten = false;
        let has_package = e
            .key
            .rsplit_once('/')
            .is_some_and(|(service, _)| service.contains('.'));
        if e.kind == ContractKind::Rpc
            && e.role != Role::Definition
            && !e.is_unresolved()
            && !has_package
            && !e.key.contains("{}")
        {
            let (service, method) = rpc_parts(&e.key);
            let candidates: Vec<&String> = definitions
                .iter()
                .filter(|d| {
                    let (s, m) = rpc_parts(d);
                    s == service && m.eq_ignore_ascii_case(&method)
                })
                .collect();
            if let [only] = candidates.as_slice() {
                copy.key = (*only).clone();
                rewritten = true;
            }
        }
        owned.push(copy);
        flags.push(rewritten);
    }
    (owned, flags)
}

/// Links the extractions of every project.
///
/// # Errors
/// [`LinkError::DuplicateProject`] when a project appears twice.
pub fn link(
    projects: &[ProjectExtractions],
    options: &LinkOptions,
) -> Result<LinkOutput, LinkError> {
    let mut seen = BTreeSet::new();
    for project in projects {
        if !seen.insert(&project.project) {
            return Err(LinkError::DuplicateProject(project.project.clone()));
        }
    }
    let (owned, suffix_matched) = canonical_rpc_keys(projects);
    let all: Vec<&Extraction> = owned.iter().collect();

    // Known keys: definitions and (non-dynamic) providers.
    let mut known = KnownKeys::default();
    let mut definitions: BTreeMap<(ContractKind, String), Vec<&Extraction>> = BTreeMap::new();
    for e in &all {
        if e.is_unresolved() {
            continue;
        }
        if e.role == Role::Definition {
            definitions
                .entry((e.kind, e.key.clone()))
                .or_default()
                .push(e);
            known
                .by_kind
                .entry(e.kind)
                .or_default()
                .insert(e.key.clone());
            if e.evidence == EvidenceType::ContractDerived {
                known.defined_by_document.insert((e.kind, e.key.clone()));
            }
        } else if e.role.is_provider(e.kind) && !e.dynamic && e.attr(ATTR_GLOB).is_none() {
            known
                .by_kind
                .entry(e.kind)
                .or_default()
                .insert(e.key.clone());
        }
    }
    let table_definitions: Vec<&Extraction> = all
        .iter()
        .copied()
        .filter(|e| e.kind == ContractKind::Table && e.role == Role::Definition)
        .collect();
    let tables = build_tables(&table_definitions);
    let entities = build_entities(&all);

    let mut contracts: BTreeMap<NodeId, Node> = BTreeMap::new();
    let mut sources: BTreeMap<Name, BTreeMap<NodeId, Node>> = BTreeMap::new();
    let mut edges: BTreeMap<Name, BTreeMap<EdgeKey, EdgeRecord>> = BTreeMap::new();

    let mut contract_node = |kind: ContractKind, key: &str, placeholder: bool| -> NodeId {
        let id = NodeId::contract(kind, key);
        if !contracts.contains_key(&id) {
            let mut node = Node::contract(kind, key);
            if placeholder {
                node = node.with_attr(ATTR_UNRESOLVED, "true");
            }
            if options.external.contains(&(kind, key.to_owned())) {
                node = node.with_attr(ATTR_EXTERNAL, "true");
            }
            contracts.insert(id.clone(), node);
        }
        id
    };

    for (index, e) in all.iter().enumerate() {
        let suffix = suffix_matched.get(index).copied().unwrap_or(false);
        let source = source_node(e);
        let source_id = source.id.clone();
        let edge_kind = e.edge_kind();
        let rule_id = format!("{}/{}", e.pack, e.rule);
        let mut targets: Vec<(NodeId, Resolution, EvidenceType, MatchKind)> = Vec::new();

        if e.role == Role::Definition {
            let id = contract_node(e.kind, &e.key, false);
            targets.push((id, Resolution::Resolved, e.evidence, MatchKind::Exact));
        } else if e.is_unresolved() {
            let id = contract_node(e.kind, &e.key, true);
            targets.push((id, Resolution::Unresolved, e.evidence, MatchKind::None));
        } else if e.role.is_provider(e.kind) && !e.dynamic && e.attr(ATTR_GLOB).is_none() {
            let id = contract_node(e.kind, &e.key, false);
            let evidence = if suffix {
                EvidenceType::Heuristic
            } else if known.defined_by_document.contains(&(e.kind, e.key.clone()))
                && e.evidence == EvidenceType::Syntactic
            {
                EvidenceType::ContractDerived
            } else {
                e.evidence
            };
            let how = if suffix {
                MatchKind::Suffix
            } else {
                MatchKind::Exact
            };
            targets.push((id, Resolution::Resolved, evidence, how));
        } else {
            let (found, how) = resolve_targets(e, &known, options);
            let how = if suffix && how == MatchKind::Exact {
                MatchKind::Suffix
            } else {
                how
            };
            let heuristic =
                e.evidence == EvidenceType::Heuristic || e.dynamic || how != MatchKind::Exact;
            match found.len() {
                0 if e.dynamic => {
                    let id = contract_node(e.kind, &e.key, true);
                    targets.push((id, Resolution::Unresolved, e.evidence, MatchKind::None));
                }
                0 => {
                    let id = contract_node(e.kind, &e.key, false);
                    targets.push((id, Resolution::Resolved, e.evidence, MatchKind::None));
                }
                count => {
                    let resolution = if count == 1 || e.attr(ATTR_GLOB) == Some("all") {
                        Resolution::Resolved
                    } else {
                        Resolution::Ambiguous
                    };
                    for key in found {
                        let evidence = if heuristic {
                            EvidenceType::Heuristic
                        } else if known.defined_by_document.contains(&(e.kind, key.clone()))
                            && e.evidence == EvidenceType::Syntactic
                        {
                            EvidenceType::ContractDerived
                        } else {
                            e.evidence
                        };
                        let id = contract_node(e.kind, &key, false);
                        targets.push((id, resolution, evidence, how));
                    }
                }
            }
        }

        sources
            .entry(e.project.clone())
            .or_default()
            .entry(source_id.clone())
            .or_insert(source);
        let project_edges = edges.entry(e.project.clone()).or_default();
        for (target, resolution, evidence, how) in targets {
            let key = EdgeKey {
                from: source_id.clone(),
                to: target.clone(),
                kind: edge_kind,
            };
            let record = project_edges.entry(key).or_insert_with(|| {
                EdgeRecord::new(
                    source_id.clone(),
                    target.clone(),
                    Edge::new(edge_kind, evidence, resolution),
                )
            });
            let edge = &mut record.edge;
            if evidence.rank() < edge.evidence.rank() {
                edge.evidence = evidence;
            }
            if resolution < edge.resolution {
                edge.resolution = resolution;
            }
            edge.evidence_refs.push(evidence_ref(e));
            edge.evidence_refs.sort();
            edge.evidence_refs.dedup();
            merge_list_attr(&mut edge.attrs, ATTR_RULES, &rule_id);
            merge_list_attr(&mut edge.attrs, ATTR_MATCH, how.as_str());
            if let Some(column) = e.attr(ATTR_COLUMN) {
                merge_list_attr(&mut edge.attrs, ATTR_COLUMNS, &column.to_lowercase());
            }
        }
    }

    // Contract attributes from definitions.
    for ((kind, key), defs) in &definitions {
        let id = NodeId::contract(*kind, key);
        let Some(node) = contracts.get_mut(&id) else {
            continue;
        };
        let mut sorted: Vec<&&Extraction> = defs.iter().collect();
        sorted.sort_by(|a, b| {
            (
                std::cmp::Reverse(numeric_version(a)),
                a.project.as_str(),
                a.path.as_str(),
                a.range.start(),
            )
                .cmp(&(
                    std::cmp::Reverse(numeric_version(b)),
                    b.project.as_str(),
                    b.path.as_str(),
                    b.range.start(),
                ))
        });
        if let Some(first) = sorted.first() {
            node.source = Some(evidence_ref(first));
            if *kind != ContractKind::Table {
                if let Some(hash) = first.attr(ATTR_SCHEMA_HASH) {
                    node.attrs
                        .insert(ATTR_SCHEMA_HASH.to_owned(), hash.to_owned());
                }
                if let Some(fields) = first.attr(ATTR_FIELDS) {
                    node.attrs.insert(ATTR_FIELDS.to_owned(), fields.to_owned());
                }
                if let Some(version) = first.attr(ATTR_VERSION) {
                    node.attrs
                        .insert(ATTR_VERSION.to_owned(), version.to_owned());
                }
            }
        }
    }
    for (name, schema) in &tables {
        let id = NodeId::contract(ContractKind::Table, name);
        if let Some(node) = contracts.get_mut(&id) {
            if let Some(hash) = schema.hash() {
                node.attrs
                    .insert(ATTR_SCHEMA_HASH.to_owned(), hash.to_owned());
            }
            let columns: Vec<String> = schema.columns().into_iter().collect();
            node.attrs
                .insert(ATTR_COLUMNS.to_owned(), columns.join(","));
        }
    }
    // The shape a table reader was built against.
    for mapping in &entities {
        let Some(schema) = tables.get(&mapping.table) else {
            continue;
        };
        if mapping.columns.is_empty() {
            continue;
        }
        let hash = match schema.built_against(&mapping.columns) {
            Some(index) => schema.versions.get(index).map(|v| v.hash.clone()),
            None => {
                let joined: Vec<&str> = mapping.columns.iter().map(String::as_str).collect();
                Some(hash_hex(
                    "table-columns/v1",
                    &[&mapping.table, &joined.join(",")],
                ))
            }
        };
        let Some(hash) = hash else {
            continue;
        };
        let from = match &mapping.symbol {
            Some(symbol) => NodeId::symbol(&mapping.project, &symbol.graph_key(&mapping.path)),
            None => NodeId::file(&mapping.project, &mapping.path),
        };
        let to = NodeId::contract(ContractKind::Table, &mapping.table);
        if let Some(project_edges) = edges.get_mut(&mapping.project) {
            for kind in [EdgeKind::Reads, EdgeKind::Writes] {
                let key = EdgeKey {
                    from: from.clone(),
                    to: to.clone(),
                    kind,
                };
                if let Some(record) = project_edges.get_mut(&key) {
                    record
                        .edge
                        .attrs
                        .insert(ATTR_SCHEMA_HASH.to_owned(), hash.clone());
                }
            }
        }
    }

    Ok(LinkOutput {
        contracts,
        sources,
        edges,
        tables,
        entities,
    })
}

fn merge_list_attr(attrs: &mut BTreeMap<String, String>, key: &str, value: &str) {
    let mut items: BTreeSet<String> = attrs
        .get(key)
        .map(|v| {
            v.split(',')
                .filter(|s| !s.is_empty())
                .map(str::to_owned)
                .collect()
        })
        .unwrap_or_default();
    items.insert(value.to_owned());
    attrs.insert(
        key.to_owned(),
        items.into_iter().collect::<Vec<_>>().join(","),
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn endpoint_scores() {
        assert_eq!(
            endpoint_score("GET /v1/orders/{}", "GET /v1/orders/{}"),
            Some(5)
        );
        assert!(endpoint_score("GET /v1/orders/123", "GET /v1/orders/{}").is_some());
        assert!(endpoint_score("GET /v1/orders/{}", "GET /v1/orders/export").is_some());
        assert_eq!(endpoint_score("GET /v1/orders", "POST /v1/orders"), None);
        assert!(endpoint_score("GET /v1/orders", "* /v1/orders").is_some());
        assert!(endpoint_score("GET /v1/orders{}", "GET /v1/orders").is_some());
        assert_eq!(
            endpoint_score("GET /v1/orders{}", "GET /v1/orders/{}"),
            None
        );
        let best = best_endpoints(
            "GET /v1/orders/export",
            [
                "GET /v1/orders/{}".to_owned(),
                "GET /v1/orders/export".to_owned(),
            ]
            .iter(),
        );
        assert_eq!(best, ["GET /v1/orders/export"]);
    }

    #[test]
    fn rpc_parts_split() {
        assert_eq!(
            rpc_parts("a.b.Svc/Get"),
            ("Svc".to_owned(), "Get".to_owned())
        );
        assert_eq!(rpc_parts("{}/Get"), ("{}".to_owned(), "Get".to_owned()));
    }

    #[test]
    fn list_attrs_merge_sorted() {
        let mut attrs = BTreeMap::new();
        merge_list_attr(&mut attrs, "rules", "b");
        merge_list_attr(&mut attrs, "rules", "a");
        merge_list_attr(&mut attrs, "rules", "b");
        assert_eq!(attrs.get("rules").map(String::as_str), Some("a,b"));
    }
}
