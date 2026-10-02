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
//! No semantic resolution happens here: shadowing, overloads across files,
//! dynamic dispatch and re-exports are not understood. That is what the
//! evidence types say.

use std::collections::{BTreeMap, BTreeSet};

use knowell_core::{LineRange, RepoPath};
use knowell_graph::EdgeKind;
use knowell_parse::tree_sitter::Node;
use knowell_parse::{Language, ParseLimits, Tier, parse_tree};
use knowell_store::graph::{NewEdge, NodeRef};
use knowell_store::symbols::{Definition, NewOccurrence};
use knowell_store::{EvidenceType, OccurrenceRole, ProjectId, Resolution, SymbolId};

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

fn plausible(name: &str) -> bool {
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
pub(crate) fn identifiers(language: Language, text: &str, limits: &ParseLimits) -> Vec<Identifier> {
    if language.tier() != Tier::Exact {
        return Vec::new();
    }
    let Some(tree) = parse_tree(language, text, limits) else {
        return Vec::new();
    };
    let bytes = text.as_bytes();
    let mut found: BTreeSet<Identifier> = BTreeSet::new();
    let mut cursor = tree.root_node().walk();
    let mut visited = 0usize;
    'walk: loop {
        visited += 1;
        if visited > MAX_NODES || found.len() >= MAX_IDENTIFIERS {
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
    found.into_iter().collect()
}

/// The reference targets of one file, by declared name.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct FileDefs {
    by_name: BTreeMap<String, BTreeSet<SymbolId>>,
}

impl FileDefs {
    /// Targets of a file analysed in this build (`ids[i]` is symbol `i`).
    pub(crate) fn from_analysis(file: &AnalysedFile, ids: &[Option<SymbolId>]) -> Self {
        let mut defs = Self::default();
        if !file.structured {
            return defs;
        }
        for (symbol, id) in file.parsed.symbols.iter().zip(ids) {
            if let Some(id) = id
                && is_target_kind(symbol.kind.as_str())
                && plausible(&symbol.name)
            {
                defs.by_name
                    .entry(symbol.name.clone())
                    .or_default()
                    .insert(*id);
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
            if plausible(name) {
                defs.by_name
                    .entry(name.to_owned())
                    .or_default()
                    .insert(definition.symbol.id);
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

/// What [`file_references`] produced for one file.
#[derive(Debug, Clone, Default, PartialEq)]
pub(crate) struct FileReferences {
    /// Occurrences with role `reference` (syntactic, resolved matches).
    pub(crate) occurrences: Vec<NewOccurrence>,
    /// `references` edges, one per (source, target).
    pub(crate) edges: Vec<NewEdge>,
}

/// The innermost symbol with an id whose lines contain `line`.
fn enclosing(file: &AnalysedFile, ids: &[Option<SymbolId>], line: u32) -> Option<SymbolId> {
    file.parsed
        .symbols
        .iter()
        .zip(ids)
        .filter_map(|(symbol, id)| {
            let id = (*id)?;
            (symbol.range.start() <= line && line <= symbol.range.end()).then_some((
                symbol.range.end().saturating_sub(symbol.range.start()),
                std::cmp::Reverse(symbol.range.start()),
                id,
            ))
        })
        .min()
        .map(|(_, _, id)| id)
}

struct Aggregate {
    first_line: u32,
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
pub(crate) fn file_references(
    project: ProjectId,
    file: &AnalysedFile,
    ids: &[Option<SymbolId>],
    own: &FileDefs,
    imported: &[&FileDefs],
    siblings: &[&FileDefs],
) -> FileReferences {
    let mut out = FileReferences::default();
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
    let mut edges: BTreeMap<(NodeRef, SymbolId), Aggregate> = BTreeMap::new();
    let mut occurrences: BTreeSet<(SymbolId, u32)> = BTreeSet::new();
    let mut cut = false;
    for ident in &file.identifiers {
        if declared.contains(&(ident.name.as_str(), ident.line)) {
            continue;
        }
        let Some(found) = resolve(&ident.name, ident.member, own, imported, siblings) else {
            continue;
        };
        let from = match enclosing(file, ids, ident.line) {
            Some(id) => NodeRef::Symbol(id),
            None => NodeRef::File {
                project,
                path: file.path.clone(),
            },
        };
        let resolution = found.resolution();
        for target in &found.candidates {
            if from == NodeRef::Symbol(*target) && ident.member {
                continue;
            }
            let key = (from.clone(), *target);
            let room = edges.len() < MAX_EDGES_PER_FILE;
            match edges.get_mut(&key) {
                Some(aggregate) => {
                    aggregate.uses = aggregate.uses.saturating_add(1);
                    // The strongest observation of the pair is kept.
                    if found.evidence == EvidenceType::Syntactic {
                        aggregate.evidence = EvidenceType::Syntactic;
                    }
                    if resolution == Resolution::Resolved {
                        aggregate.resolution = Resolution::Resolved;
                    }
                }
                None if room => {
                    edges.insert(
                        key,
                        Aggregate {
                            first_line: ident.line,
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
        tracing::debug!(path = %file.path, "references cut at the per-file bound");
    }
    let origin = file.path.to_string();
    for ((from, to), aggregate) in edges {
        let mut evidence = serde_json::json!({
            "path": file.path.as_str(),
            "content_hash": file.content_hash.to_string(),
            "lines": [aggregate.first_line, aggregate.first_line],
            "name": aggregate.name,
            "scope": aggregate.scope.as_str(),
            "uses": aggregate.uses,
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
            kind: EdgeKind::References.as_str().to_owned(),
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
        // Single-character names are never matched.
        assert!(!names(&rust).iter().any(|(n, _, _)| *n == "x"));
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
        let own = file_references(project, &util, &util_ids, &util_defs, &[], &[]);
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
}
