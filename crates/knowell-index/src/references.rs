//! Conservative same-project reference resolution for exact-tier languages
//! (Rust, TypeScript / JavaScript, Python, Go, Java, Kotlin, C#).
//!
//! `knowell-parse` reports declarations and imports but not identifier uses,
//! so T1 parses an exact-tier file a second time ([`identifiers`]) to list
//! the identifiers it uses, outside comments and strings. A use is matched
//! **by name** against the definitions visible from the file, in this order
//! (the first scope with a match wins):
//!
//! | Scope | Candidates | Evidence |
//! |---|---|---|
//! | `file` | definitions in the same file | `syntactic` for a free name, `heuristic` for a member name (`x.name`) |
//! | `import` | definitions in files the file imports (import edges resolved to files of the view) | as above |
//! | `directory` | definitions in other files of the same directory and language family | `heuristic` |
//!
//! One candidate gives resolution `resolved`; two to [`MAX_CANDIDATES`] give
//! `ambiguous` (one edge per candidate); more are dropped as too ambiguous to
//! be useful. Only declarations that are referenced by name are targets
//! (functions, methods, types, constants, macros; not fields, variables,
//! modules, `impl` blocks or constructors, whose names repeat type names).
//!
//! Every match becomes a `references` edge from the innermost enclosing
//! symbol (or the file) to the target, carrying the evidence type and the
//! resolution separately. Only `syntactic` + `resolved` matches also become
//! occurrences with role `reference`, so an occurrence never presents a
//! guess as a reference. Work per file is bounded ([`MAX_IDENTIFIERS`],
//! [`MAX_NODES`], [`MAX_EDGES_PER_FILE`], [`MAX_OCCURRENCES_PER_FILE`]); what
//! is cut is logged, never silently presented as complete.
//!
//! Rust additionally retains byte spans, lexical bindings, use roles and
//! explicit module/import aliases. Only actual Rust callee occurrences can
//! produce `calls`. Unknown receivers, macros, unsupported imports and
//! duplicate declaration identities remain unresolved. This is source-level
//! analysis; compiler configuration, dispatch and re-exports require SCIP.

use std::collections::{BTreeMap, BTreeSet};

use knowell_core::{LineRange, RepoPath};
use knowell_graph::EdgeKind;
use knowell_parse::tree_sitter::Node;
use knowell_parse::{Language, ParseLimits, Tier, parse_tree};
use knowell_store::graph::{NewEdge, NodeRef};
use knowell_store::symbols::{Definition, NewOccurrence};
use knowell_store::{EvidenceType, OccurrenceRole, ProjectId, Resolution, SymbolId};
use serde::{Deserialize, Serialize};

use crate::analyze::{AnalysedFile, split_symbol_key};

/// Most distinct identifier uses (name, line, member) kept per file.
pub(crate) const MAX_IDENTIFIERS: usize = 20_000;
/// Most syntax nodes visited per file.
pub(crate) const MAX_NODES: usize = 2_000_000;
/// Most candidates of an ambiguous match; more are dropped.
pub(crate) const MAX_CANDIDATES: usize = 4;
/// Most `references` edges written per file.
pub(crate) const MAX_EDGES_PER_FILE: usize = 1_000;
/// Most reference occurrences written per file.
pub(crate) const MAX_OCCURRENCES_PER_FILE: usize = 2_000;
/// Most imported files whose definitions are candidates.
pub(crate) const MAX_IMPORTED_FILES: usize = 64;
/// Directories with more files of the same language family than this are
/// not used as a candidate scope (a flat directory says little).
pub(crate) const MAX_SIBLING_FILES: usize = 256;

/// One use of an identifier.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) struct Identifier {
    /// The name as written.
    pub(crate) name: String,
    /// 1-based line.
    pub(crate) line: u32,
    /// Accessed as a member (`x.name`, `x->name`, `X::name` is not one).
    pub(crate) member: bool,
    /// Exact 0-based, half-open source byte span of the name.
    pub(crate) start_byte: usize,
    pub(crate) end_byte: usize,
    /// True only for the name in a call expression's callee position.
    pub(crate) is_call: bool,
    /// A syntactic path or receiver, with `.` separating segments.
    pub(crate) qualified: Option<String>,
    pub(crate) kind: UseKind,
    /// A local lexical binding owns this name; compiler ingestion may still resolve it.
    pub(crate) shadowed: bool,
    /// Lexical owner of a declaration name; source bytes, half-open.
    pub(crate) scope_start_byte: usize,
    pub(crate) scope_end_byte: usize,
}

/// Namespace/role retained independently of a possible target.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum UseKind {
    Value,
    Type,
    Macro,
    Qualifier,
    Declaration,
}

/// Bounded syntax/reference work for one source version. Zero resolved edges
/// never implies complete compiler analysis.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub(crate) struct ReferenceCoverage {
    pub(crate) scanned: usize,
    pub(crate) call_sites: usize,
    pub(crate) resolved: usize,
    pub(crate) ambiguous: usize,
    pub(crate) unresolved: usize,
    pub(crate) shadowed: usize,
    pub(crate) truncated: bool,
    pub(crate) parse_unavailable: bool,
    pub(crate) imports_unresolved: usize,
    pub(crate) candidate_overflow: usize,
    pub(crate) calls_resolved: usize,
    pub(crate) calls_ambiguous: usize,
    pub(crate) calls_unresolved: usize,
    pub(crate) calls_written: usize,
    pub(crate) calls_complete: bool,
}

#[derive(Debug, Clone, Default)]
pub(crate) struct IdentifierScan {
    pub(crate) identifiers: Vec<Identifier>,
    pub(crate) coverage: ReferenceCoverage,
}

/// Groups languages whose files reference each other's declarations.
pub(crate) fn family(language: Language) -> Option<&'static str> {
    match language {
        Language::TypeScript | Language::Tsx | Language::JavaScript | Language::Jsx => Some("js"),
        Language::Kotlin | Language::Java => Some("jvm"),
        other if other.tier() == Tier::Exact => Some(other.as_str()),
        _ => None,
    }
}

/// The language family of a path, from its name alone.
pub(crate) fn path_family(path: &RepoPath) -> Option<&'static str> {
    Language::from_path(path).and_then(family)
}

/// Whether a symbol of this kind is a reference target.
pub(crate) fn is_target_kind(kind: &str) -> bool {
    matches!(
        kind,
        "function"
            | "method"
            | "class"
            | "struct"
            | "enum"
            | "interface"
            | "trait"
            | "type_alias"
            | "constant"
            | "macro"
    )
}

pub(crate) fn plausible(name: &str) -> bool {
    let mut chars = name.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    (2..=128).contains(&name.len())
        && (first.is_alphabetic() || first == '_' || first == '$')
        && chars.all(|c| c.is_alphanumeric() || c == '_' || c == '$')
}

/// Whether the leaf `node`, which appears under its parent as `field`, is
/// accessed as a member of something else.
fn is_member(node: &Node<'_>, field: Option<&str>) -> bool {
    if matches!(node.kind(), "property_identifier" | "field_identifier") {
        return true;
    }
    if matches!(field, Some("property" | "field" | "attribute")) {
        return true;
    }
    let Some(parent) = node.parent() else {
        return false;
    };
    match parent.kind() {
        "navigation_suffix" => true,
        "method_invocation" | "field_access" | "member_access_expression" => {
            field == Some("name")
                && (parent.child_by_field_name("object").is_some()
                    || parent.child_by_field_name("expression").is_some())
        }
        _ => false,
    }
}

/// The identifier uses of an exact-tier file, sorted and de-duplicated, from
/// a bounded second parse. Empty for other tiers and for text the parser
/// rejects (too large, minified, too deep, timed out).
pub(crate) fn identifier_scan(
    language: Language,
    text: &str,
    limits: &ParseLimits,
) -> IdentifierScan {
    if language.tier() != Tier::Exact {
        return IdentifierScan::default();
    }
    let Some(tree) = parse_tree(language, text, limits) else {
        return IdentifierScan {
            coverage: ReferenceCoverage {
                parse_unavailable: true,
                ..ReferenceCoverage::default()
            },
            ..IdentifierScan::default()
        };
    };
    if language == Language::Rust {
        return crate::rust_references::scan(tree.root_node(), text);
    }
    let mut coverage = ReferenceCoverage::default();
    let bytes = text.as_bytes();
    let mut found: BTreeSet<Identifier> = BTreeSet::new();
    let mut cursor = tree.root_node().walk();
    let mut visited = 0usize;
    'walk: loop {
        visited += 1;
        if visited > MAX_NODES || found.len() >= MAX_IDENTIFIERS {
            coverage.truncated = true;
            tracing::debug!(
                visited,
                kept = found.len(),
                "identifier scan cut at its bound"
            );
            break;
        }
        let node = cursor.node();
        let field = cursor.field_name();
        if node.child_count() == 0
            && node.is_named()
            && node.kind().ends_with("identifier")
            && !matches!(node.kind(), "package_identifier" | "namespace_identifier")
            // Keys of object / dictionary literals are not uses.
            && field != Some("key")
            && let Ok(name) = node.utf8_text(bytes)
            && plausible(name)
        {
            let line = u32::try_from(node.start_position().row)
                .unwrap_or(u32::MAX)
                .saturating_add(1);
            found.insert(Identifier {
                name: name.to_owned(),
                line,
                member: is_member(&node, field),
                start_byte: node.start_byte(),
                end_byte: node.end_byte(),
                is_call: generic_callee(node),
                qualified: None,
                kind: if node.kind() == "type_identifier" {
                    UseKind::Type
                } else {
                    UseKind::Value
                },
                shadowed: false,
                scope_start_byte: 0,
                scope_end_byte: text.len(),
            });
        }
        if cursor.goto_first_child() {
            continue;
        }
        loop {
            if cursor.goto_next_sibling() {
                continue 'walk;
            }
            if !cursor.goto_parent() {
                break 'walk;
            }
        }
    }
    let identifiers: Vec<Identifier> = found.into_iter().collect();
    coverage.scanned = identifiers.len();
    coverage.call_sites = identifiers.iter().filter(|i| i.is_call).count();
    IdentifierScan {
        identifiers,
        coverage,
    }
}

/// Compatibility helper used by extraction tests; production retains coverage.
#[cfg(test)]
fn identifiers(language: Language, text: &str, limits: &ParseLimits) -> Vec<Identifier> {
    identifier_scan(language, text, limits).identifiers
}

fn generic_callee(node: Node<'_>) -> bool {
    let Some(parent) = node.parent() else {
        return false;
    };
    if matches!(parent.kind(), "call_expression" | "call") {
        return parent
            .child_by_field_name("function")
            .is_some_and(|f| f.id() == node.id());
    }
    if matches!(
        parent.kind(),
        "member_expression" | "attribute" | "selector_expression"
    ) {
        let named = parent
            .child_by_field_name("property")
            .or_else(|| parent.child_by_field_name("attribute"))
            .or_else(|| parent.child_by_field_name("field"));
        return named.is_some_and(|f| f.id() == node.id())
            && parent.parent().is_some_and(|p| {
                matches!(p.kind(), "call_expression" | "call")
                    && p.child_by_field_name("function")
                        .is_some_and(|f| f.id() == parent.id())
            });
    }
    matches!(parent.kind(), "method_invocation")
        && parent
            .child_by_field_name("name")
            .is_some_and(|n| n.id() == node.id())
}

/// The reference targets of one file, by declared name.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct FileDefs {
    by_name: BTreeMap<String, BTreeSet<SymbolId>>,
    by_qualified: BTreeMap<String, BTreeSet<SymbolId>>,
    path: Option<RepoPath>,
    targets: BTreeMap<SymbolId, Target>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Target {
    kind: String,
    scope: Option<std::ops::Range<usize>>,
    duplicate: bool,
}

fn rust_name(name: &str) -> String {
    name.split('.')
        .map(|p| p.strip_prefix("r#").unwrap_or(p))
        .collect::<Vec<_>>()
        .join(".")
}

impl FileDefs {
    /// Targets of a file analysed in this build (`ids[i]` is symbol `i`).
    pub(crate) fn from_analysis(file: &AnalysedFile, ids: &[Option<SymbolId>]) -> Self {
        let mut defs = Self {
            path: Some(file.path.clone()),
            ..Self::default()
        };
        if !file.structured {
            return defs;
        }
        let declaration_scopes: BTreeMap<(String, usize), (usize, usize)> = file
            .identifiers
            .iter()
            .filter(|i| i.kind == UseKind::Declaration)
            .map(|i| {
                (
                    (i.name.clone(), i.start_byte),
                    (i.scope_start_byte, i.scope_end_byte),
                )
            })
            .collect();
        for (symbol, id) in file.parsed.symbols.iter().zip(ids) {
            if let Some(id) = id
                && is_target_kind(symbol.kind.as_str())
                && (plausible(&symbol.name) || file.parsed.language == Language::Rust)
            {
                defs.by_name
                    .entry(symbol.name.clone())
                    .or_default()
                    .insert(*id);
                let name = rust_name(&symbol.name);
                let mut scope = declaration_scopes
                    .range((name.clone(), symbol.byte_range.start)..(name, symbol.byte_range.end))
                    .next()
                    .map(|(_, (start, end))| *start..*end);
                // An associated method's impl body is not its visibility scope:
                // `Type::method` may be referenced outside that body.
                if symbol.kind.as_str() == "method" {
                    scope = None;
                }
                let qualified = if file.parsed.language == Language::Rust {
                    rust_name(&symbol.qualified_name)
                } else {
                    symbol.qualified_name.clone()
                };
                defs.by_qualified
                    .entry(qualified.clone())
                    .or_default()
                    .insert(*id);
                let duplicate = defs.targets.contains_key(id);
                defs.targets.insert(
                    *id,
                    Target {
                        kind: symbol.kind.as_str().to_owned(),
                        scope,
                        duplicate,
                    },
                );
            }
        }
        defs
    }

    /// Targets of a file read back from the store. The declared name is the
    /// last segment of the in-file qualified name.
    pub(crate) fn from_definitions<'a>(definitions: impl Iterator<Item = &'a Definition>) -> Self {
        let mut defs = Self::default();
        for definition in definitions {
            if !is_target_kind(&definition.symbol.kind) {
                continue;
            }
            let Some((_, local)) = split_symbol_key(&definition.symbol.qualified_name) else {
                continue;
            };
            let name = local.rsplit('.').next().unwrap_or(local);
            if plausible(name) || definition.path.extension() == Some("rs") {
                defs.path = Some(definition.path.clone());
                defs.by_name
                    .entry(name.to_owned())
                    .or_default()
                    .insert(definition.symbol.id);
                let duplicate = defs.targets.contains_key(&definition.symbol.id);
                let qualified = if definition.path.extension() == Some("rs") {
                    rust_name(local)
                } else {
                    local.to_owned()
                };
                defs.by_qualified
                    .entry(qualified.clone())
                    .or_default()
                    .insert(definition.symbol.id);
                defs.targets.insert(
                    definition.symbol.id,
                    Target {
                        kind: definition.symbol.kind.clone(),
                        scope: None,
                        duplicate,
                    },
                );
            }
        }
        defs
    }

    fn get(&self, name: &str) -> Option<&BTreeSet<SymbolId>> {
        self.by_name.get(name).filter(|ids| !ids.is_empty())
    }

    /// Whether the file has no targets.
    pub(crate) fn is_empty(&self) -> bool {
        self.by_name.is_empty()
    }
}

/// Where a match was found.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Scope {
    File,
    Import,
    Directory,
}

impl Scope {
    fn as_str(self) -> &'static str {
        match self {
            Scope::File => "file",
            Scope::Import => "import",
            Scope::Directory => "directory",
        }
    }
}

/// The candidates of one name.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Match {
    pub(crate) candidates: Vec<SymbolId>,
    pub(crate) evidence: EvidenceType,
    pub(crate) scope: Scope,
}

impl Match {
    fn resolution(&self) -> Resolution {
        if self.candidates.len() == 1 {
            Resolution::Resolved
        } else {
            Resolution::Ambiguous
        }
    }
}

/// Resolves one use against the scopes in order (see the module docs).
pub(crate) fn resolve(
    name: &str,
    member: bool,
    own: &FileDefs,
    imported: &[&FileDefs],
    siblings: &[&FileDefs],
) -> Option<Match> {
    let named = if member {
        EvidenceType::Heuristic
    } else {
        EvidenceType::Syntactic
    };
    let scopes: [(Scope, Vec<&FileDefs>, EvidenceType); 3] = [
        (Scope::File, vec![own], named),
        (Scope::Import, imported.to_vec(), named),
        (Scope::Directory, siblings.to_vec(), EvidenceType::Heuristic),
    ];
    for (scope, files, evidence) in scopes {
        let candidates: BTreeSet<SymbolId> = files
            .iter()
            .filter_map(|defs| defs.get(name))
            .flatten()
            .copied()
            .collect();
        if candidates.is_empty() {
            continue;
        }
        if candidates.len() > MAX_CANDIDATES {
            return None;
        }
        return Some(Match {
            candidates: candidates.into_iter().collect(),
            evidence,
            scope,
        });
    }
    None
}

fn name_candidate_overflow(
    name: &str,
    own: &FileDefs,
    imported: &[&FileDefs],
    siblings: &[&FileDefs],
) -> bool {
    for files in [std::slice::from_ref(&own), imported, siblings] {
        let mut candidates = BTreeSet::new();
        for defs in files {
            for id in defs.get(name).into_iter().flatten() {
                candidates.insert(*id);
                if candidates.len() > MAX_CANDIDATES {
                    return true;
                }
            }
        }
        if !candidates.is_empty() {
            return false;
        }
    }
    false
}

/// What [`file_references`] produced for one file.
#[derive(Debug, Clone, Default, PartialEq)]
pub(crate) struct FileReferences {
    /// Occurrences with role `reference` (syntactic, resolved matches).
    pub(crate) occurrences: Vec<NewOccurrence>,
    /// `references` edges, one per (source, target).
    pub(crate) edges: Vec<NewEdge>,
    pub(crate) coverage: ReferenceCoverage,
}

/// The innermost symbol with an id whose lines contain `line`.
fn enclosing(file: &AnalysedFile, ids: &[Option<SymbolId>], start_byte: usize) -> Option<SymbolId> {
    file.parsed
        .symbols
        .iter()
        .zip(ids)
        .filter_map(|(symbol, id)| {
            let id = (*id)?;
            symbol.byte_range.contains(&start_byte).then_some((
                symbol
                    .byte_range
                    .end
                    .saturating_sub(symbol.byte_range.start),
                std::cmp::Reverse(symbol.byte_range.start),
                id,
            ))
        })
        .min()
        .map(|(_, _, id)| id)
}

#[derive(Default)]
struct RustContext {
    namespace: Option<String>,
    container: Option<String>,
    functions: Vec<String>,
    owner: Option<SymbolId>,
}

/// A source-order sweep avoids rescanning every declaration for every use.
fn rust_contexts(file: &AnalysedFile, ids: &[Option<SymbolId>]) -> BTreeMap<usize, RustContext> {
    let mut events = Vec::new();
    for (index, symbol) in file.parsed.symbols.iter().enumerate() {
        if symbol.byte_range.start < symbol.byte_range.end {
            events.push((symbol.byte_range.start, true, index));
            events.push((symbol.byte_range.end, false, index));
        }
    }
    events.sort_unstable();
    let mut offsets: Vec<_> = file.identifiers.iter().map(|i| i.start_byte).collect();
    offsets.sort_unstable();
    offsets.dedup();
    let mut active = BTreeSet::new();
    let mut event_index = 0usize;
    let mut out = BTreeMap::new();
    for offset in offsets {
        while events.get(event_index).is_some_and(|e| e.0 <= offset) {
            if let Some((_, start, index)) = events.get(event_index)
                && let Some(symbol) = file.parsed.symbols.get(*index)
            {
                let key = (
                    symbol
                        .byte_range
                        .end
                        .saturating_sub(symbol.byte_range.start),
                    std::cmp::Reverse(symbol.byte_range.start),
                    *index,
                );
                if *start {
                    active.insert(key);
                } else {
                    active.remove(&key);
                }
            }
            event_index = event_index.saturating_add(1);
        }
        let mut context = RustContext::default();
        for (_, _, index) in &active {
            let Some(symbol) = file.parsed.symbols.get(*index) else {
                continue;
            };
            if context.owner.is_none() {
                context.owner = ids.get(*index).copied().flatten();
            }
            match symbol.kind.as_str() {
                "module" if context.namespace.is_none() => {
                    context.namespace = Some(symbol.qualified_name.clone())
                }
                "impl" | "trait" if context.container.is_none() => {
                    context.container = Some(symbol.qualified_name.clone())
                }
                "function" | "method" => context.functions.push(symbol.qualified_name.clone()),
                _ => {}
            }
        }
        out.insert(offset, context);
    }
    out
}

fn accepts_rust_target(target: &Target, ident: &Identifier) -> bool {
    match ident.kind {
        UseKind::Type => matches!(
            target.kind.as_str(),
            "struct" | "enum" | "trait" | "type_alias"
        ),
        UseKind::Value => {
            if ident.member && !ident.is_call {
                return false;
            }
            matches!(
                target.kind.as_str(),
                "function" | "method" | "constant" | "struct" | "enum"
            ) && (!ident.is_call || target.kind != "constant")
        }
        _ => false,
    }
}

fn exact_rust_targets(
    defs: &FileDefs,
    qualified: &str,
    ident: &Identifier,
    check_scope: bool,
) -> BTreeSet<SymbolId> {
    defs.by_qualified
        .get(qualified)
        .into_iter()
        .flatten()
        .filter_map(|id| {
            let target = defs.targets.get(id)?;
            (!target.duplicate
                && accepts_rust_target(target, ident)
                && (!check_scope
                    || target
                        .scope
                        .as_ref()
                        .is_none_or(|scope| scope.contains(&ident.start_byte))))
            .then_some(*id)
        })
        .collect()
}

fn rust_match(candidates: BTreeSet<SymbolId>, scope: Scope) -> Option<Match> {
    (!candidates.is_empty() && candidates.len() <= MAX_CANDIDATES).then(|| Match {
        candidates: candidates.into_iter().collect(),
        evidence: EvidenceType::Syntactic,
        scope,
    })
}

/// Rust uses are resolved against observed lexical namespaces and import bindings,
/// never all same-named declarations in a sibling directory.
fn resolve_rust(
    file: &AnalysedFile,
    ident: &Identifier,
    own: &FileDefs,
    imported: &[&FileDefs],
    context: Option<(&crate::rust_imports::RustImports, &BTreeSet<RepoPath>)>,
    lexical: &RustContext,
) -> Option<Match> {
    if ident.shadowed
        || matches!(
            ident.kind,
            UseKind::Macro | UseKind::Qualifier | UseKind::Declaration
        )
    {
        return None;
    }
    if ident.member
        && !ident
            .qualified
            .as_deref()
            .is_some_and(|s| s.starts_with("self."))
    {
        return None;
    }
    let qualified = ident.qualified.as_deref();
    if let Some(path) = qualified
        && let Some(suffix) = path
            .strip_prefix("Self.")
            .or_else(|| path.strip_prefix("self."))
    {
        let container = lexical.container.as_deref()?;
        return rust_match(
            exact_rust_targets(
                own,
                &rust_name(&format!("{container}.{suffix}")),
                ident,
                true,
            ),
            Scope::File,
        );
    }
    let mut nested_matches = BTreeSet::new();
    if qualified.is_none() {
        // A nested item is visible in its enclosing function, not other functions.
        for container in &lexical.functions {
            let matches = exact_rust_targets(
                own,
                &rust_name(&format!("{container}.{}", ident.name)),
                ident,
                true,
            );
            if !matches.is_empty() {
                nested_matches = matches;
                break;
            }
        }
    }
    let local = qualified.unwrap_or(&ident.name);
    let local = rust_name(
        &lexical
            .namespace
            .as_ref()
            .map_or_else(|| local.to_owned(), |ns| format!("{ns}.{local}")),
    );
    let own_matches = if nested_matches.is_empty() {
        exact_rust_targets(own, &local, ident, true)
    } else {
        nested_matches
    };
    let segments: Vec<String> = qualified
        .unwrap_or(&ident.name)
        .split('.')
        .map(str::to_owned)
        .collect();
    if let Some((imports, files)) = context
        && let Some(first) = segments.first()
        && imports.has_binding(first, ident.start_byte)
    {
        let (path, target) =
            imports.qualified_target(&file.path, files, &segments, ident.start_byte)?;
        let defs = std::iter::once(own)
            .chain(imported.iter().copied())
            .find(|defs| defs.path.as_ref() == Some(&path))?;
        let mut alias_matches = exact_rust_targets(defs, &target, ident, false);
        let alias_extent = imports
            .bindings
            .iter()
            .filter(|b| b.local_name == *first && b.scope.contains(&ident.start_byte))
            .map(|b| b.scope.end.saturating_sub(b.scope.start))
            .chain(
                imports
                    .modules
                    .iter()
                    .filter(|m| m.local_name == *first && m.scope.contains(&ident.start_byte))
                    .map(|m| m.scope.end.saturating_sub(m.scope.start)),
            )
            .min();
        let own_extent = own_matches
            .iter()
            .filter_map(|id| own.targets.get(id)?.scope.as_ref())
            .map(|scope| scope.end.saturating_sub(scope.start))
            .min();
        match (own_extent, alias_extent) {
            (Some(own_extent), Some(alias_extent)) if own_extent < alias_extent => {
                return rust_match(own_matches, Scope::File);
            }
            (Some(own_extent), Some(alias_extent)) if own_extent == alias_extent => {
                alias_matches.extend(own_matches)
            }
            _ => {}
        }
        return rust_match(alias_matches, Scope::Import);
    }
    if !own_matches.is_empty() {
        return rust_match(own_matches, Scope::File);
    }
    let (imports, files) = context?;
    let (path, target) =
        imports.qualified_target(&file.path, files, &segments, ident.start_byte)?;
    let defs = std::iter::once(own)
        .chain(imported.iter().copied())
        .find(|defs| defs.path.as_ref() == Some(&path))?;
    rust_match(
        exact_rust_targets(defs, &target, ident, false),
        Scope::Import,
    )
}

struct Aggregate {
    first_line: u32,
    start_byte: usize,
    end_byte: usize,
    uses: u32,
    name: String,
    scope: Scope,
    candidates: usize,
    evidence: EvidenceType,
    resolution: Resolution,
}

/// The references of one analysed file (see the module docs). `ids[i]` is
/// the id of symbol `i`; `own`, `imported` and `siblings` are the candidate
/// scopes.
#[allow(clippy::too_many_arguments)]
pub(crate) fn file_references(
    project: ProjectId,
    file: &AnalysedFile,
    ids: &[Option<SymbolId>],
    own: &FileDefs,
    imported: &[&FileDefs],
    siblings: &[&FileDefs],
    rust: Option<(&crate::rust_imports::RustImports, &BTreeSet<RepoPath>)>,
) -> FileReferences {
    let mut out = FileReferences {
        coverage: file.reference_coverage.clone(),
        ..FileReferences::default()
    };
    if let Some((imports, _)) = rust {
        out.coverage.imports_unresolved = imports.unresolved_count;
        out.coverage.truncated |= imports.truncated;
    }
    if !file.structured || file.identifiers.is_empty() {
        return out;
    }
    // A declaration's own name is not a use of it.
    let declared: BTreeSet<(&str, u32)> = file
        .parsed
        .symbols
        .iter()
        .map(|s| (s.name.as_str(), s.name_line))
        .collect();
    let mut edges: BTreeMap<(NodeRef, SymbolId, &'static str), Aggregate> = BTreeMap::new();
    let mut occurrences: BTreeSet<(SymbolId, u32)> = BTreeSet::new();
    let mut cut = false;
    let rust_contexts = (file.parsed.language == Language::Rust).then(|| rust_contexts(file, ids));
    for ident in &file.identifiers {
        let is_rust = file.parsed.language == Language::Rust;
        if (is_rust && matches!(ident.kind, UseKind::Declaration | UseKind::Qualifier))
            || (!is_rust && declared.contains(&(ident.name.as_str(), ident.line)))
        {
            continue;
        }
        if ident.shadowed {
            if ident.is_call {
                out.coverage.unresolved = out.coverage.unresolved.saturating_add(1);
                out.coverage.calls_unresolved = out.coverage.calls_unresolved.saturating_add(1);
            }
            continue;
        }
        let lexical = rust_contexts
            .as_ref()
            .and_then(|c| c.get(&ident.start_byte));
        let found = if is_rust {
            lexical.and_then(|lexical| resolve_rust(file, ident, own, imported, rust, lexical))
        } else {
            resolve(&ident.name, ident.member, own, imported, siblings)
        };
        let Some(found) = found else {
            if !is_rust && name_candidate_overflow(&ident.name, own, imported, siblings) {
                out.coverage.candidate_overflow = out.coverage.candidate_overflow.saturating_add(1);
            }
            out.coverage.unresolved = out.coverage.unresolved.saturating_add(1);
            if ident.is_call {
                out.coverage.calls_unresolved = out.coverage.calls_unresolved.saturating_add(1);
            }
            continue;
        };
        let owner = if let Some(lexical) = lexical {
            lexical.owner
        } else {
            enclosing(file, ids, ident.start_byte)
        };
        let from = match owner {
            Some(id) => NodeRef::Symbol(id),
            None => NodeRef::File {
                project,
                path: file.path.clone(),
            },
        };
        let resolution = found.resolution();
        match resolution {
            Resolution::Resolved => out.coverage.resolved = out.coverage.resolved.saturating_add(1),
            Resolution::Ambiguous => {
                out.coverage.ambiguous = out.coverage.ambiguous.saturating_add(1)
            }
            Resolution::Unresolved => {
                out.coverage.unresolved = out.coverage.unresolved.saturating_add(1)
            }
        }
        if ident.is_call {
            match resolution {
                Resolution::Resolved => {
                    out.coverage.calls_resolved = out.coverage.calls_resolved.saturating_add(1)
                }
                Resolution::Ambiguous => {
                    out.coverage.calls_ambiguous = out.coverage.calls_ambiguous.saturating_add(1)
                }
                Resolution::Unresolved => {
                    out.coverage.calls_unresolved = out.coverage.calls_unresolved.saturating_add(1)
                }
            }
        }
        let relation = if is_rust && ident.is_call {
            EdgeKind::Calls.as_str()
        } else {
            EdgeKind::References.as_str()
        };
        for target in &found.candidates {
            if from == NodeRef::Symbol(*target) && ident.member && !is_rust {
                continue;
            }
            let key = (from.clone(), *target, relation);
            let room = edges.len() < MAX_EDGES_PER_FILE;
            match edges.get_mut(&key) {
                Some(aggregate) => {
                    aggregate.uses = aggregate.uses.saturating_add(1);
                    // Keep one actual observation's axes and coordinates together;
                    // combining independent maxima could invent an unsupported claim.
                    if (found.evidence, resolution) < (aggregate.evidence, aggregate.resolution) {
                        aggregate.first_line = ident.line;
                        aggregate.start_byte = ident.start_byte;
                        aggregate.end_byte = ident.end_byte;
                        aggregate.name = ident.name.clone();
                        aggregate.scope = found.scope;
                        aggregate.candidates = found.candidates.len();
                        aggregate.evidence = found.evidence;
                        aggregate.resolution = resolution;
                    }
                }
                None if room => {
                    edges.insert(
                        key,
                        Aggregate {
                            first_line: ident.line,
                            start_byte: ident.start_byte,
                            end_byte: ident.end_byte,
                            uses: 1,
                            name: ident.name.clone(),
                            scope: found.scope,
                            candidates: found.candidates.len(),
                            evidence: found.evidence,
                            resolution,
                        },
                    );
                }
                None => cut = true,
            }
        }
        if found.evidence == EvidenceType::Syntactic
            && resolution == Resolution::Resolved
            && let Some(target) = found.candidates.first()
        {
            if occurrences.len() < MAX_OCCURRENCES_PER_FILE {
                occurrences.insert((*target, ident.line));
            } else {
                cut = true;
            }
        }
    }
    if cut {
        out.coverage.truncated = true;
        tracing::debug!(path = %file.path, "references cut at the per-file bound");
    }
    let origin = file.path.to_string();
    for ((from, to, relation), aggregate) in edges {
        if relation == "calls" {
            out.coverage.calls_written = out.coverage.calls_written.saturating_add(1);
        }
        let mut evidence = serde_json::json!({
            "path": file.path.as_str(),
            "content_hash": file.content_hash.to_string(),
            "lines": [aggregate.first_line, aggregate.first_line],
            "name": aggregate.name,
            "scope": aggregate.scope.as_str(),
            "uses": aggregate.uses,
            "bytes": [aggregate.start_byte, aggregate.end_byte],
        });
        if aggregate.candidates > 1
            && let Some(map) = evidence.as_object_mut()
        {
            map.insert(
                "candidates".to_owned(),
                serde_json::json!(aggregate.candidates),
            );
        }
        out.edges.push(NewEdge {
            from,
            to: NodeRef::Symbol(to),
            kind: relation.to_owned(),
            evidence_type: aggregate.evidence,
            resolution: aggregate.resolution,
            evidence,
            origin: origin.clone(),
        });
    }
    for (symbol, line) in occurrences {
        let Ok(lines) = LineRange::new(line, line) else {
            continue;
        };
        out.occurrences.push(NewOccurrence {
            symbol,
            path: file.path.clone(),
            content_hash: file.content_hash,
            lines,
            role: OccurrenceRole::Reference,
        });
    }
    out
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::AtomicBool;

    use knowell_core::ContentHash;
    use knowell_parse::ChunkOptions;

    use super::*;
    use crate::analyze::{AnalyseOptions, analyse};
    use crate::config::GeneratedPolicy;

    fn p(s: &str) -> RepoPath {
        RepoPath::new(s).unwrap()
    }

    fn id(n: u128) -> SymbolId {
        SymbolId(uuid::Uuid::from_u128(n))
    }

    fn analysed(path: &str, text: &str) -> AnalysedFile {
        let text: Arc<str> = Arc::from(text);
        analyse(
            "billing",
            &p(path),
            ContentHash::of(text.as_bytes()),
            text,
            &AnalyseOptions {
                chunking: ChunkOptions::default(),
                limits: ParseLimits::default(),
                generated: GeneratedPolicy::SkipEmbeddings,
                identifiers: true,
            },
            &AtomicBool::new(false),
        )
    }

    fn names(list: &[Identifier]) -> Vec<(&str, u32, bool)> {
        list.iter()
            .map(|i| (i.name.as_str(), i.line, i.member))
            .collect()
    }

    #[test]
    fn identifiers_skip_comments_strings_and_keys() {
        let text = "// helper in a comment\nconst s = 'helper in a string';\nconst o = { helper: 1 };\nexport function run() { return helper(2) + o.size; }\n";
        let found = identifiers(Language::TypeScript, text, &ParseLimits::default());
        let helper: Vec<_> = names(&found)
            .into_iter()
            .filter(|(n, _, _)| *n == "helper")
            .collect();
        assert_eq!(helper, [("helper", 4, false)]);
        assert!(names(&found).contains(&("size", 4, true)));
        // Not an exact-tier language: nothing.
        assert!(identifiers(Language::Ruby, "def a; b; end", &ParseLimits::default()).is_empty());
        assert!(identifiers(Language::TypeScript, "", &ParseLimits::default()).is_empty());
    }

    #[test]
    fn identifiers_of_several_languages() {
        let limits = ParseLimits::default();
        let rust = identifiers(
            Language::Rust,
            "fn a() { helper(); x.field_name(); }\n",
            &limits,
        );
        assert!(names(&rust).contains(&("helper", 1, false)));
        // Single-character callable names are retained too; declarations are
        // distinguished by source role rather than identifier length.
        assert!(
            rust.iter()
                .any(|i| i.name == "a" && i.kind == UseKind::Declaration)
        );
        assert!(names(&rust).contains(&("field_name", 1, true)));
        let python = identifiers(
            Language::Python,
            "def a():\n    return obj.attr_name(cancel)\n",
            &limits,
        );
        assert!(names(&python).contains(&("attr_name", 2, true)));
        assert!(names(&python).contains(&("cancel", 2, false)));
        let go = identifiers(
            Language::Go,
            "package x\nfunc A() { Bee(); s.Cancel() }\n",
            &limits,
        );
        assert!(names(&go).contains(&("Bee", 2, false)));
        assert!(names(&go).contains(&("Cancel", 2, true)));
        let java = identifiers(
            Language::Java,
            "class A { void a() { helper(); svc.cancel(); } }\n",
            &limits,
        );
        assert!(names(&java).contains(&("helper", 1, false)));
        assert!(names(&java).contains(&("cancel", 1, true)));
    }

    #[test]
    fn hostile_input_is_bounded() {
        let limits = ParseLimits::default();
        // Many distinct identifiers: the scan stops at its bound.
        let text: String = (0..(MAX_IDENTIFIERS + 50))
            .map(|i| format!("let v{i} = w{i};\n"))
            .collect();
        let found = identifiers(Language::TypeScript, &text, &limits);
        assert!(found.len() <= MAX_IDENTIFIERS + 1);
        // Broken syntax still yields what parses.
        let broken = identifiers(Language::TypeScript, "function ( { helper(", &limits);
        assert!(broken.len() <= 2);
        // Deep nesting is rejected before parsing.
        let deep = format!("{}{}", "(".repeat(5_000), ")".repeat(5_000));
        assert!(identifiers(Language::TypeScript, &deep, &limits).is_empty());
    }

    #[test]
    fn scopes_are_tried_in_order_and_ambiguity_is_bounded() {
        let mut own = FileDefs::default();
        own.by_name.insert("local".into(), [id(1)].into());
        let mut util = FileDefs::default();
        util.by_name.insert("helper".into(), [id(2)].into());
        util.by_name.insert("format".into(), [id(3)].into());
        let mut other = FileDefs::default();
        other.by_name.insert("format".into(), [id(4)].into());
        let mut sibling = FileDefs::default();
        sibling.by_name.insert("near".into(), [id(5)].into());
        sibling.by_name.insert("helper".into(), [id(6)].into());
        let imported = [&util, &other];
        let siblings = [&sibling];

        let local = resolve("local", false, &own, &imported, &siblings).unwrap();
        assert_eq!(local.scope, Scope::File);
        assert_eq!(local.evidence, EvidenceType::Syntactic);
        // An import wins over a sibling with the same name.
        let helper = resolve("helper", false, &own, &imported, &siblings).unwrap();
        assert_eq!(
            (helper.scope, helper.candidates.clone()),
            (Scope::Import, vec![id(2)])
        );
        assert_eq!(helper.resolution(), Resolution::Resolved);
        let format = resolve("format", false, &own, &imported, &siblings).unwrap();
        assert_eq!(format.candidates, vec![id(3), id(4)]);
        assert_eq!(format.resolution(), Resolution::Ambiguous);
        let near = resolve("near", false, &own, &imported, &siblings).unwrap();
        assert_eq!(
            (near.scope, near.evidence),
            (Scope::Directory, EvidenceType::Heuristic)
        );
        let member = resolve("helper", true, &own, &imported, &siblings).unwrap();
        assert_eq!(member.evidence, EvidenceType::Heuristic);
        assert!(resolve("unknown", false, &own, &imported, &siblings).is_none());
        let mut crowded = FileDefs::default();
        crowded.by_name.insert(
            "get".into(),
            (10..10 + MAX_CANDIDATES as u128 + 1).map(id).collect(),
        );
        assert!(resolve("get", false, &crowded, &[], &[]).is_none());
    }

    #[test]
    fn references_become_edges_and_resolved_syntactic_occurrences() {
        let util = analysed(
            "src/util.ts",
            "export function helper(x: number): number { return x; }\nexport function format(v: string): string { return v; }\n",
        );
        let other = analysed(
            "src/other.ts",
            "export function format(v: string): string { return v.trim(); }\n",
        );
        let main = analysed(
            "src/main.ts",
            "import { helper, format } from './util';\nimport { format as f2 } from './other';\n\nexport function run(): number {\n  format('a');\n  return helper(1);\n}\n",
        );
        assert!(!main.identifiers.is_empty());
        let ids = |file: &AnalysedFile, base: u128| -> Vec<Option<SymbolId>> {
            (0..file.parsed.symbols.len())
                .map(|i| Some(id(base + i as u128)))
                .collect()
        };
        let (util_ids, other_ids, main_ids) = (ids(&util, 100), ids(&other, 200), ids(&main, 300));
        let util_defs = FileDefs::from_analysis(&util, &util_ids);
        let other_defs = FileDefs::from_analysis(&other, &other_ids);
        let main_defs = FileDefs::from_analysis(&main, &main_ids);
        let project = ProjectId(uuid::Uuid::nil());
        let refs = file_references(
            project,
            &main,
            &main_ids,
            &main_defs,
            &[&util_defs, &other_defs],
            &[],
            None,
        );
        let run = NodeRef::Symbol(id(300));
        let helper = id(100);
        let helper_edge = refs
            .edges
            .iter()
            .find(|e| e.to == NodeRef::Symbol(helper) && e.from == run)
            .expect("run references helper");
        assert_eq!(helper_edge.evidence_type, EvidenceType::Syntactic);
        assert_eq!(helper_edge.resolution, Resolution::Resolved);
        assert_eq!(helper_edge.origin, "src/main.ts");
        assert_eq!(helper_edge.kind, "references");
        let format_edges: Vec<&NewEdge> = refs
            .edges
            .iter()
            .filter(|e| e.from == run && e.evidence["name"] == "format")
            .collect();
        assert_eq!(format_edges.len(), 2);
        assert!(
            format_edges
                .iter()
                .all(|e| e.resolution == Resolution::Ambiguous)
        );
        // Occurrences only for the unambiguous syntactic match.
        assert!(
            refs.occurrences
                .iter()
                .all(|o| o.symbol == helper && o.role == OccurrenceRole::Reference)
        );
        assert!(refs.occurrences.iter().any(|o| o.lines.start() == 6));
        // The declaration's own name is not a reference.
        let own = file_references(project, &util, &util_ids, &util_defs, &[], &[], None);
        assert!(own.edges.is_empty() && own.occurrences.is_empty());
    }

    #[test]
    fn definitions_from_the_store_use_the_last_name_segment() {
        let symbol = |name: &str, kind: &str| Definition {
            symbol: knowell_store::symbols::Symbol {
                id: id(7),
                project: ProjectId(uuid::Uuid::nil()),
                qualified_name: name.to_owned(),
                kind: kind.to_owned(),
                created_at: time::OffsetDateTime::UNIX_EPOCH,
                updated_at: time::OffsetDateTime::UNIX_EPOCH,
            },
            path: p("a.go"),
            content_hash: ContentHash::of(b"a"),
            lines: LineRange::new(1, 1).unwrap(),
        };
        let list = [
            symbol("a.go#Service.Cancel", "method"),
            symbol("a.go#field", "field"),
            symbol("broken", "function"),
        ];
        let defs = FileDefs::from_definitions(list.iter());
        assert!(defs.get("Cancel").is_some());
        assert!(defs.get("field").is_none());
        assert!(defs.get("broken").is_none());
        assert!(FileDefs::default().is_empty());
        assert_eq!(path_family(&p("a.tsx")), path_family(&p("b.js")));
        assert_ne!(path_family(&p("a.rs")), path_family(&p("b.go")));
        assert_eq!(path_family(&p("a.md")), None);
    }

    fn rust_refs(text: &str) -> FileReferences {
        let file = analysed("src/lib.rs", text);
        let ids: Vec<_> = (0..file.parsed.symbols.len())
            .map(|i| Some(id(1_000 + i as u128)))
            .collect();
        let defs = FileDefs::from_analysis(&file, &ids);
        let files = BTreeSet::from([file.path.clone()]);
        let imports = crate::rust_imports::resolve(&file.path, text, &files, &file.parse_limits);
        file_references(
            ProjectId(uuid::Uuid::nil()),
            &file,
            &ids,
            &defs,
            &[],
            &[],
            Some((&imports, &files)),
        )
    }

    #[test]
    fn rust_local_raw_and_layout_do_not_reference_unrelated_items() {
        let refs = rust_refs(
            "struct BitWriter; impl BitWriter { fn raw(&self) {} }\nfn layout() {}\nfn run() { let raw = 1; let layout = 2; consume(raw, layout); }\n",
        );
        assert!(
            refs.edges
                .iter()
                .all(|e| e.evidence["name"] != "raw" && e.evidence["name"] != "layout")
        );
        assert!(refs.coverage.shadowed >= 2);
        assert!(!refs.coverage.calls_complete);
    }

    #[test]
    fn rust_initializer_and_same_line_recursion_keep_actual_calls() {
        let refs = rust_refs(
            "fn layout() {} fn run() { let layout = layout(); consume(layout); } fn repeat() { repeat(); } fn f() { f(); }\n",
        );
        for name in ["layout", "repeat", "f"] {
            assert_eq!(
                refs.edges
                    .iter()
                    .filter(|e| e.kind == "calls" && e.evidence["name"] == name)
                    .count(),
                1,
                "{name}"
            );
        }
        assert!(
            refs.edges
                .iter()
                .any(|e| e.kind == "calls" && e.from == e.to)
        );
        assert_eq!(refs.coverage.calls_written, 3);
    }

    #[test]
    fn rust_parameter_closure_loop_and_match_bindings_suppress_call_guesses() {
        let refs = rust_refs(
            "fn helper() {} fn run(helper: fn()) { helper(); let closure = |helper: fn()| helper(); for helper in items() { helper(); } match value() { Some(helper) => helper(), _ => {} } }\n",
        );
        assert!(
            refs.edges
                .iter()
                .all(|e| e.kind != "calls" || e.evidence["name"] != "helper")
        );
        assert!(refs.coverage.calls_unresolved >= 4);
    }

    #[test]
    fn rust_calls_require_callee_syntax_and_a_supported_receiver() {
        let refs = rust_refs(
            "struct Writer; impl Writer { fn raw(&self) {} fn run(&self) { self.raw(); other.raw(); let value = other.raw; } }\n",
        );
        let calls: Vec<_> = refs
            .edges
            .iter()
            .filter(|e| e.kind == "calls" && e.evidence["name"] == "raw")
            .collect();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].evidence_type, EvidenceType::Syntactic);
        assert_eq!(calls[0].resolution, Resolution::Resolved);
        assert!(refs.coverage.calls_unresolved >= 1);
    }

    #[test]
    fn rust_nested_item_does_not_escape_its_lexical_block() {
        let refs = rust_refs("fn run() { { fn helper() {} helper(); } helper(); }\n");
        assert_eq!(
            refs.edges
                .iter()
                .filter(|e| e.kind == "calls" && e.evidence["name"] == "helper")
                .count(),
            1
        );
        assert_eq!(refs.coverage.calls_unresolved, 1);
    }

    #[test]
    fn rust_block_import_alias_is_visible_before_its_declaration() {
        let main = analysed(
            "src/lib.rs",
            "mod helpers; fn local() {} fn run() { { local(); use crate::helpers::helper as local; } local(); }\n",
        );
        let helper = analysed("src/helpers.rs", "pub fn helper() {}\n");
        let main_ids: Vec<_> = (0..main.parsed.symbols.len())
            .map(|i| Some(id(2_000 + i as u128)))
            .collect();
        let helper_ids = vec![Some(id(3_000))];
        let own = FileDefs::from_analysis(&main, &main_ids);
        let imported = FileDefs::from_analysis(&helper, &helper_ids);
        let files = BTreeSet::from([main.path.clone(), helper.path.clone()]);
        let imports =
            crate::rust_imports::resolve(&main.path, &main.text, &files, &main.parse_limits);
        let refs = file_references(
            ProjectId(uuid::Uuid::nil()),
            &main,
            &main_ids,
            &own,
            &[&imported],
            &[],
            Some((&imports, &files)),
        );
        assert!(
            refs.edges
                .iter()
                .any(|e| e.kind == "calls" && e.to == NodeRef::Symbol(id(3_000)))
        );
        assert_eq!(
            refs.edges
                .iter()
                .filter(|e| e.kind == "calls" && e.evidence["name"] == "local")
                .count(),
            2
        );
    }

    #[test]
    fn strongest_reference_cites_the_observation_that_supports_it() {
        let file = analysed(
            "src/run.ts",
            "function helper() {}\nfunction run() {\n object.helper();\n helper();\n}\n",
        );
        let ids: Vec<_> = (0..file.parsed.symbols.len())
            .map(|i| Some(id(4_000 + i as u128)))
            .collect();
        let defs = FileDefs::from_analysis(&file, &ids);
        let refs = file_references(
            ProjectId(uuid::Uuid::nil()),
            &file,
            &ids,
            &defs,
            &[],
            &[],
            None,
        );
        let edge = refs
            .edges
            .iter()
            .find(|e| e.evidence["name"] == "helper")
            .unwrap();
        assert_eq!(edge.evidence_type, EvidenceType::Syntactic);
        assert_eq!(edge.evidence["lines"], serde_json::json!([4, 4]));
        assert_eq!(edge.evidence["uses"], 2);
        let start = edge.evidence["bytes"][0].as_u64().unwrap() as usize;
        let end = edge.evidence["bytes"][1].as_u64().unwrap() as usize;
        assert_eq!(file.text.get(start..end), Some("helper"));
    }
}
