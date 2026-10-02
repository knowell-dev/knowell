//! Running rule packs over the files of a project.
//!
//! One pass over the files collects query matches and bindings; a second,
//! in-memory step resolves constants and lookups (which may be defined in
//! other files) and renders keys. Files are never parsed twice.
//!
//! Safety properties:
//! - sensitive paths (`.env*`, keys, credentials, ...) are rejected by
//!   [`knowell_secrets::ExclusionPolicy`] before their content is read;
//! - readable content is redacted (secret-shaped spans replaced, line
//!   numbers kept) before any rule sees it;
//! - parsing and query execution are bounded by [`ParseLimits`] and a match
//!   cap; nothing panics on malformed input.

mod decode;
mod detect;
mod redact;
mod route;
pub(crate) mod sql;

use std::collections::{BTreeMap, BTreeSet};
use std::ops::ControlFlow;
use std::time::Instant;

use knowell_core::{ContentHash, LineRange, Name, RepoPath};
use knowell_graph::ContractKind;
use knowell_parse::tree_sitter::{
    Node, Query, QueryCursor, QueryCursorOptions, QueryCursorState, StreamingIterator, Tree,
};
use knowell_parse::{Language, ParseLimits, ParsedFile, SymbolKind};
use knowell_secrets::ExclusionPolicy;

use crate::model::{
    ATTR_GLOB, ATTR_HOST, ATTR_UNRESOLVED_KEY, DYN, Extraction, ProjectExtractions, Role, Shape,
    SkippedFile, SymbolRef,
};
use crate::normalize::{Normalized, normalize, render_dynamic};
use crate::pack::template::{Template, last_segment};
use crate::pack::{BindingScope, KeyShape, Pack, PackSet, PostProcess, Rule};
use crate::structured;
pub(crate) use decode::unquote as decode_unquote;
use decode::{Piece, Pieces, decode_expr, decode_plain};
use detect::{ProjectDeps, imports_match, manifest_kind, project_matches, read_manifest};

/// Bounds and policies of extraction.
#[derive(Clone)]
pub struct ExtractOptions {
    /// Size, time and nesting bounds for every parse and query run.
    pub limits: ParseLimits,
    /// Most query matches kept per file and rule (pathological files).
    pub max_matches_per_file: usize,
    /// Paths that must never be read (built-in rules plus user patterns).
    pub exclusion: ExclusionPolicy,
    /// Struct / class field sets recorded per project at most.
    pub max_shapes: usize,
}

impl Default for ExtractOptions {
    fn default() -> Self {
        Self {
            limits: ParseLimits::default(),
            max_matches_per_file: 5_000,
            exclusion: ExclusionPolicy::builtin(),
            max_shapes: 50_000,
        }
    }
}

impl std::fmt::Debug for ExtractOptions {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ExtractOptions")
            .field("limits", &self.limits)
            .field("max_matches_per_file", &self.max_matches_per_file)
            .field("max_shapes", &self.max_shapes)
            .finish_non_exhaustive()
    }
}

/// The value of one captured node.
#[derive(Debug, Clone)]
struct CaptureValue {
    plain: String,
    expr: Option<Pieces>,
}

type Captures = BTreeMap<String, Vec<CaptureValue>>;

/// A rule match whose rendering waits for constants and lookups.
#[derive(Debug)]
struct Pending {
    pack: usize,
    rule: usize,
    file: usize,
    pattern: usize,
    anchor: (usize, usize),
    range: LineRange,
    symbol: Option<SymbolRef>,
    enclosing: Option<String>,
    captures: Captures,
    route: Option<String>,
}

/// A binding match.
#[derive(Debug)]
struct BindingEntry {
    pack: usize,
    binding: usize,
    file: usize,
    enclosing: Option<String>,
    name: String,
    captures: Captures,
}

/// Per-file facts kept for the resolution step.
#[derive(Debug)]
struct FileFacts {
    path: RepoPath,
    content_hash: ContentHash,
}

/// Results of running one pack over one file (pack fixture tests).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PackRun {
    /// Extractions, sorted.
    pub extractions: Vec<Extraction>,
    /// Bindings the pack's binding queries found, as
    /// `bind <id> <name> = <value> | <value>` lines, sorted.
    pub bindings: Vec<String>,
}

/// Extracts the contracts of one project.
///
/// `paths` are the project's files (repository-relative); `read` returns a
/// file's text and is only called for paths that pass `options.exclusion`.
/// Unreadable, excluded and oversized files are listed in
/// [`ProjectExtractions::skipped`]. Packs are activated per project by their
/// detection hints (dependency manifests, files) or per file by import
/// hints.
pub fn extract_project(
    project: &Name,
    paths: &[RepoPath],
    packs: &PackSet,
    options: &ExtractOptions,
    read: &mut dyn FnMut(&RepoPath) -> Option<String>,
) -> ProjectExtractions {
    let mut paths: Vec<RepoPath> = paths.to_vec();
    paths.sort();
    paths.dedup();
    let mut skipped = Vec::new();
    let mut readable = Vec::new();
    for path in paths {
        match options.exclusion.check(&path) {
            Some(exclusion) => skipped.push(SkippedFile {
                reason: format!("excluded: {}", exclusion_reason(&exclusion)),
                path,
            }),
            None => readable.push(path),
        }
    }

    // Manifests first: they decide which packs are active.
    let mut deps = ProjectDeps::default();
    let mut cache: BTreeMap<RepoPath, Option<String>> = BTreeMap::new();
    for path in &readable {
        if let Some(kind) = manifest_kind(path) {
            let text = read(path);
            if let Some(text) = &text
                && text.len() <= options.limits.max_bytes
            {
                read_manifest(kind, text, &mut deps);
            }
            cache.insert(path.clone(), text);
        }
    }
    let project_active: Vec<bool> = packs
        .packs()
        .iter()
        .map(|pack| project_matches(&pack.detect, &deps, &readable))
        .collect();

    let mut rule_active = BTreeSet::new();
    for (pack_index, pack) in packs.packs().iter().enumerate() {
        for (rule_index, rule) in pack.rules.iter().enumerate() {
            if let Some(detect) = &rule.detect
                && (detect.always || detect.files.is_some() || !detect.deps.is_empty())
                && project_matches(detect, &deps, &readable)
            {
                rule_active.insert((pack_index, rule_index));
            }
        }
    }
    let mut run = Run::new(packs.packs().iter().collect(), rule_active, options);
    for path in readable {
        let text = match cache.remove(&path) {
            Some(text) => text,
            None => read(&path),
        };
        let Some(text) = text else {
            skipped.push(SkippedFile {
                path,
                reason: "unreadable".to_owned(),
            });
            continue;
        };
        if text.len() > options.limits.max_bytes {
            skipped.push(SkippedFile {
                path,
                reason: format!("too_large: {} bytes", text.len()),
            });
            continue;
        }
        run.file(project, &path, &text, &project_active, false);
    }
    let mut result = run.finish(project);
    skipped.sort_by(|a, b| a.path.cmp(&b.path));
    result.skipped = skipped;
    result
}

/// Runs `pack` over one file, forcing it active (its import and dependency
/// hints are ignored; rule-level hints still apply). `support` packs (for
/// example the bundled `constants` pack) are also forced active so their
/// bindings resolve identifiers, but only `pack`'s extractions and bindings
/// are returned. Used by the pack fixture tests and pack authoring tools.
pub fn run_pack_on_file(
    pack: &Pack,
    support: &[&Pack],
    project: &Name,
    path: &RepoPath,
    text: &str,
) -> PackRun {
    let options = ExtractOptions::default();
    let mut packs: Vec<&Pack> = vec![pack];
    packs.extend(support.iter().copied().filter(|p| p.name() != pack.name()));
    let mut rule_active = BTreeSet::new();
    for (pack_index, candidate) in packs.iter().enumerate() {
        for (rule_index, rule) in candidate.rules.iter().enumerate() {
            if rule.detect.as_ref().is_some_and(|d| {
                (d.always || d.files.is_some())
                    && project_matches(d, &ProjectDeps::default(), std::slice::from_ref(path))
            }) {
                rule_active.insert((pack_index, rule_index));
            }
        }
    }
    let active = vec![true; packs.len()];
    let mut run = Run::new(packs, rule_active, &options);
    run.file(project, path, text, &active, true);
    let bindings = run.binding_lines(0);
    let pack_id = pack.id();
    let mut result = run.finish(project);
    result.extractions.retain(|e| e.pack == pack_id);
    PackRun {
        extractions: result.extractions,
        bindings,
    }
}

fn exclusion_reason(exclusion: &knowell_secrets::Exclusion) -> String {
    match exclusion {
        knowell_secrets::Exclusion::Sensitive(kind) => kind.as_str().to_owned(),
        knowell_secrets::Exclusion::Internal => "internal".to_owned(),
        knowell_secrets::Exclusion::Pattern(_) => "pattern".to_owned(),
    }
}

/// State of one extraction run.
struct Run<'p> {
    packs: Vec<&'p Pack>,
    /// `(pack, rule)` whose rule-level hints matched at project level.
    rule_active: BTreeSet<(usize, usize)>,
    options: &'p ExtractOptions,
    files: Vec<FileFacts>,
    pending: Vec<Pending>,
    bindings: Vec<BindingEntry>,
    direct: Vec<Extraction>,
    shapes: Vec<Shape>,
    used_packs: BTreeSet<String>,
}

impl<'p> Run<'p> {
    fn new(
        packs: Vec<&'p Pack>,
        rule_active: BTreeSet<(usize, usize)>,
        options: &'p ExtractOptions,
    ) -> Self {
        Self {
            packs,
            rule_active,
            options,
            files: Vec::new(),
            pending: Vec::new(),
            bindings: Vec::new(),
            direct: Vec::new(),
            shapes: Vec::new(),
            used_packs: BTreeSet::new(),
        }
    }

    /// Analyses one file. With `force`, exactly the packs flagged in
    /// `project_active` run and import hints are ignored (fixture tests).
    fn file(
        &mut self,
        project: &Name,
        path: &RepoPath,
        original: &str,
        project_active: &[bool],
        force: bool,
    ) {
        let content_hash = ContentHash::of(original.as_bytes());
        let text = redact::redact_keep_lines(original);
        let language = Language::detect(path, &text);
        let file_index = self.files.len();
        self.files.push(FileFacts {
            path: path.clone(),
            content_hash,
        });

        let packs = self.packs.clone();
        let has_grammar = knowell_parse::ts_language(language).is_some();
        // Symbols and imports: needed for attribution, import hints and shapes.
        let parsed: Option<ParsedFile> = (has_grammar && is_code(language))
            .then(|| knowell_parse::parse_with(path, &text, &self.options.limits, None));
        let imports: Vec<String> = parsed
            .as_ref()
            .map(|p| p.imports.iter().map(|i| i.specifier.clone()).collect())
            .unwrap_or_default();

        let mut tree: Option<Option<Tree>> = None;
        for (pack_index, pack) in packs.iter().enumerate() {
            let project_level = project_active.get(pack_index).copied().unwrap_or(false);
            let active = project_level
                || (!force
                    && !pack.detect.imports.is_empty()
                    && imports_match(&pack.detect, &imports));
            if !active {
                continue;
            }
            let mut used = false;
            // Structured extractors.
            for extractor in &pack.extractors {
                if !glob_ok(extractor.files.as_ref(), extractor.exclude.as_ref(), path) {
                    continue;
                }
                let ctx = structured::Ctx {
                    project,
                    path,
                    content_hash,
                    pack: pack.id(),
                    rule: &extractor.id,
                    limits: &self.options.limits,
                };
                let found = structured::run(extractor.kind, &ctx, &text);
                used |= !found.is_empty();
                self.direct.extend(found);
            }
            // Bindings.
            for (binding_index, binding) in pack.bindings.iter().enumerate() {
                let Some(query) = binding.queries.get(&language) else {
                    continue;
                };
                if !glob_ok(binding.files.as_ref(), None, path) {
                    continue;
                }
                let Some(tree) = tree
                    .get_or_insert_with(|| {
                        knowell_parse::parse_tree(language, &text, &self.options.limits)
                    })
                    .as_ref()
                else {
                    continue;
                };
                for m in run_query(query, tree, &text, self.options) {
                    let Some(name) = m
                        .captures
                        .get(&binding.name)
                        .and_then(|nodes| nodes.first())
                        .map(|(node, _)| decode_plain(*node, &text))
                    else {
                        continue;
                    };
                    let anchor = m.span;
                    let enclosing =
                        enclosing_symbol(parsed.as_ref(), anchor.0).map(|s| s.qualified_name);
                    let captures = capture_values(&m, &text, &binding.resolve);
                    self.bindings.push(BindingEntry {
                        pack: pack_index,
                        binding: binding_index,
                        file: file_index,
                        enclosing,
                        name,
                        captures,
                    });
                    used = true;
                }
            }
            // Rules.
            for (rule_index, rule) in pack.rules.iter().enumerate() {
                let Some(query) = rule.queries.get(&language) else {
                    continue;
                };
                if !glob_ok(rule.files.as_ref(), rule.exclude.as_ref(), path) {
                    continue;
                }
                if let Some(detect) = &rule.detect
                    && !self.rule_active.contains(&(pack_index, rule_index))
                    && !imports_match(detect, &imports)
                {
                    continue;
                }
                let route = match rule.route {
                    Some(convention) => match route::route_for(convention, path.as_str()) {
                        Some(route) => Some(route),
                        None => continue,
                    },
                    None => None,
                };
                let Some(tree) = tree
                    .get_or_insert_with(|| {
                        knowell_parse::parse_tree(language, &text, &self.options.limits)
                    })
                    .as_ref()
                else {
                    continue;
                };
                let mut found: Vec<Pending> = Vec::new();
                for m in run_query(query, tree, &text, self.options) {
                    let anchor_node = rule
                        .anchor
                        .as_ref()
                        .and_then(|a| m.captures.get(a))
                        .and_then(|nodes| nodes.first())
                        .map(|(node, _)| *node);
                    let (anchor, range) = match anchor_node {
                        Some(node) => ((node.start_byte(), node.end_byte()), node_lines(node)),
                        None => (m.span, m.lines),
                    };
                    let Some(range) = range else {
                        continue;
                    };
                    let enclosing = enclosing_symbol(parsed.as_ref(), anchor.0);
                    let symbol = match &rule.symbol {
                        Some(capture) => m
                            .captures
                            .get(capture)
                            .and_then(|nodes| nodes.first())
                            .and_then(|(node, _)| {
                                named_symbol(
                                    parsed.as_ref(),
                                    &decode_plain(*node, &text),
                                    node.start_byte(),
                                )
                            })
                            .or_else(|| enclosing.clone()),
                        None => enclosing.clone(),
                    };
                    found.push(Pending {
                        pack: pack_index,
                        rule: rule_index,
                        file: file_index,
                        pattern: m.pattern,
                        anchor,
                        range,
                        symbol,
                        enclosing: enclosing.map(|s| s.qualified_name),
                        captures: capture_values(&m, &text, &rule.resolve),
                        route: route.clone(),
                    });
                }
                if rule.anchor.is_some() {
                    found = dedupe_by_anchor(found);
                }
                used |= !found.is_empty();
                self.pending.extend(found);
            }
            if used {
                self.used_packs.insert(pack.id());
            }
        }
        if let Some(parsed) = &parsed
            && self.shapes.len() < self.options.max_shapes
        {
            self.shapes.extend(shapes(parsed, content_hash));
        }
    }

    /// `bind <id> <name> = <values>` lines for the binding entries of the
    /// pack at `pack_index`.
    fn binding_lines(&self, pack_index: usize) -> Vec<String> {
        let resolver = Resolver::new(self);
        let mut lines: Vec<String> = (0..self.bindings.len())
            .filter_map(|index| {
                let entry = self.bindings.get(index)?;
                if entry.pack != pack_index {
                    return None;
                }
                let pack = self.packs.get(entry.pack).copied()?;
                let binding = pack.bindings.get(entry.binding)?;
                let values: Vec<String> = resolver
                    .binding_values(index, 0)
                    .iter()
                    .map(|v| render_dynamic(v))
                    .collect();
                Some(format!(
                    "bind {} {} = {}",
                    binding.id,
                    entry.name,
                    values.join(" | ")
                ))
            })
            .collect();
        lines.sort();
        lines.dedup();
        lines
    }

    fn finish(self, project: &Name) -> ProjectExtractions {
        let resolver = Resolver::new(&self);
        let mut extractions = self.direct.clone();
        let mut by_file: BTreeMap<usize, Vec<Extraction>> = BTreeMap::new();
        for pending in &self.pending {
            let found = resolver.render(project, pending);
            by_file.entry(pending.file).or_default().extend(found);
        }
        for (_, found) in by_file {
            extractions.extend(apply_requires(&self, found));
        }
        sort_extractions(&mut extractions);
        let mut shapes = self.shapes.clone();
        shapes.sort_by(|a, b| {
            (&a.path, &a.symbol.qualified_name).cmp(&(&b.path, &b.symbol.qualified_name))
        });
        shapes.dedup();
        ProjectExtractions {
            project: project.clone(),
            extractions,
            skipped: Vec::new(),
            shapes,
            packs: self.used_packs.iter().cloned().collect(),
        }
    }
}

/// Languages whose files carry code symbols (not data or documents).
fn is_code(language: Language) -> bool {
    !matches!(
        language,
        Language::Yaml
            | Language::Json
            | Language::Toml
            | Language::Markdown
            | Language::Sql
            | Language::Css
    )
}

fn glob_ok(
    files: Option<&globset::GlobSet>,
    exclude: Option<&globset::GlobSet>,
    path: &RepoPath,
) -> bool {
    files.is_none_or(|g| g.is_match(path.as_str()))
        && !exclude.is_some_and(|g| g.is_match(path.as_str()))
}

/// One query match, detached from the cursor.
struct Match<'t> {
    pattern: usize,
    captures: BTreeMap<String, Vec<(Node<'t>, usize)>>,
    span: (usize, usize),
    lines: Option<LineRange>,
}

fn node_lines(node: Node<'_>) -> Option<LineRange> {
    let start = u32::try_from(node.start_position().row)
        .ok()?
        .checked_add(1)?;
    let end = u32::try_from(node.end_position().row)
        .ok()?
        .checked_add(1)?;
    LineRange::new(start, end.max(start)).ok()
}

fn run_query<'t>(
    query: &Query,
    tree: &'t Tree,
    text: &str,
    options: &ExtractOptions,
) -> Vec<Match<'t>> {
    let names = query.capture_names();
    let mut cursor = QueryCursor::new();
    cursor.set_match_limit(4_096);
    let deadline = Instant::now() + options.limits.timeout;
    let mut progress = |_: &QueryCursorState| {
        if Instant::now() > deadline {
            ControlFlow::Break(())
        } else {
            ControlFlow::Continue(())
        }
    };
    let query_options = QueryCursorOptions::new().progress_callback(&mut progress);
    let mut matches =
        cursor.matches_with_options(query, tree.root_node(), text.as_bytes(), query_options);
    let mut out = Vec::new();
    while let Some(m) = matches.next() {
        if out.len() >= options.max_matches_per_file {
            break;
        }
        let mut captures: BTreeMap<String, Vec<(Node<'t>, usize)>> = BTreeMap::new();
        let mut start = usize::MAX;
        let mut end = 0usize;
        let mut first_row = usize::MAX;
        let mut last_row = 0usize;
        for capture in m.captures() {
            let Some(name) = names.get(capture.index as usize) else {
                continue;
            };
            let node = capture.node;
            if !name.starts_with('_') {
                start = start.min(node.start_byte());
                end = end.max(node.end_byte());
                first_row = first_row.min(node.start_position().row);
                last_row = last_row.max(node.end_position().row);
            }
            captures
                .entry((*name).to_owned())
                .or_default()
                .push((node, node.start_byte()));
        }
        if start == usize::MAX {
            continue;
        }
        for nodes in captures.values_mut() {
            nodes.sort_by_key(|(_, offset)| *offset);
        }
        let lines = (|| {
            let a = u32::try_from(first_row).ok()?.checked_add(1)?;
            let b = u32::try_from(last_row).ok()?.checked_add(1)?;
            LineRange::new(a, b.max(a)).ok()
        })();
        out.push(Match {
            pattern: m.pattern_index,
            captures,
            span: (start, end),
            lines,
        });
    }
    out
}

fn capture_values(m: &Match<'_>, text: &str, resolve: &BTreeSet<String>) -> Captures {
    m.captures
        .iter()
        .map(|(name, nodes)| {
            let values = nodes
                .iter()
                .map(|(node, _)| CaptureValue {
                    plain: decode_plain(*node, text),
                    expr: resolve.contains(name).then(|| decode_expr(*node, text)),
                })
                .collect();
            (name.clone(), values)
        })
        .collect()
}

/// Keeps, per anchor, the match with the most captured nodes (optional
/// captures), ties going to the earliest pattern.
fn dedupe_by_anchor(found: Vec<Pending>) -> Vec<Pending> {
    let mut best: BTreeMap<(usize, usize), Pending> = BTreeMap::new();
    for pending in found {
        let count = |p: &Pending| p.captures.values().map(Vec::len).sum::<usize>();
        match best.get(&pending.anchor) {
            Some(existing)
                if count(existing) > count(&pending)
                    || (count(existing) == count(&pending)
                        && existing.pattern <= pending.pattern) => {}
            _ => {
                best.insert(pending.anchor, pending);
            }
        }
    }
    best.into_values().collect()
}

fn symbol_ref(symbol: &knowell_parse::Symbol) -> SymbolRef {
    SymbolRef {
        qualified_name: symbol.qualified_name.clone(),
        range: symbol.range,
    }
}

/// Innermost symbol containing `offset`.
fn enclosing_symbol(parsed: Option<&ParsedFile>, offset: usize) -> Option<SymbolRef> {
    parsed?
        .symbols
        .iter()
        .filter(|s| {
            s.byte_range.start <= offset && offset < s.byte_range.end.max(s.byte_range.start + 1)
        })
        .min_by_key(|s| {
            (
                s.byte_range.end.saturating_sub(s.byte_range.start),
                usize::MAX - s.byte_range.start,
            )
        })
        .map(symbol_ref)
}

/// Innermost symbol named `name` containing `offset`.
fn named_symbol(parsed: Option<&ParsedFile>, name: &str, offset: usize) -> Option<SymbolRef> {
    parsed?
        .symbols
        .iter()
        .filter(|s| s.name == name && s.byte_range.start <= offset && offset < s.byte_range.end)
        .min_by_key(|s| s.byte_range.end.saturating_sub(s.byte_range.start))
        .map(symbol_ref)
}

fn shapes(parsed: &ParsedFile, content_hash: ContentHash) -> Vec<Shape> {
    let mut out = Vec::new();
    for (index, symbol) in parsed.symbols.iter().enumerate() {
        if !matches!(
            symbol.kind,
            SymbolKind::Struct | SymbolKind::Class | SymbolKind::Interface
        ) {
            continue;
        }
        let mut fields: Vec<String> = parsed
            .symbols
            .iter()
            .filter(|s| s.parent == Some(index) && s.kind == SymbolKind::Field)
            .map(|s| s.name.clone())
            .collect();
        if fields.is_empty() {
            continue;
        }
        fields.sort();
        fields.dedup();
        out.push(Shape {
            path: parsed.path.clone(),
            content_hash,
            symbol: symbol_ref(symbol),
            fields,
        });
    }
    out
}

/// Drops extractions of rules with `requires_rule` when the required rule
/// found nothing for the same symbol in the same file.
fn apply_requires(run: &Run<'_>, found: Vec<Extraction>) -> Vec<Extraction> {
    let markers: BTreeSet<(String, String, Option<String>)> = found
        .iter()
        .map(|e| {
            (
                e.pack.clone(),
                e.rule.clone(),
                e.symbol.as_ref().map(|s| s.qualified_name.clone()),
            )
        })
        .collect();
    found
        .into_iter()
        .filter(|e| {
            let Some(pack) = run.packs.iter().find(|p| p.id() == e.pack) else {
                return true;
            };
            let Some(rule) = pack.rules.iter().find(|r| r.id == e.rule) else {
                return true;
            };
            match &rule.requires_rule {
                None => true,
                Some(required) => markers.contains(&(
                    e.pack.clone(),
                    required.clone(),
                    e.symbol.as_ref().map(|s| s.qualified_name.clone()),
                )),
            }
        })
        .collect()
}

pub(crate) fn sort_extractions(extractions: &mut Vec<Extraction>) {
    extractions.sort_by(|a, b| {
        (
            a.project.as_str(),
            a.path.as_str(),
            a.range.start(),
            a.kind,
            &a.key,
            a.role,
            &a.symbol,
            &a.rule,
            &a.attrs,
        )
            .cmp(&(
                b.project.as_str(),
                b.path.as_str(),
                b.range.start(),
                b.kind,
                &b.key,
                b.role,
                &b.symbol,
                &b.rule,
                &b.attrs,
            ))
    });
    extractions.dedup();
}

/// Resolves constants and lookups and renders pending matches.
struct Resolver<'r, 'p> {
    run: &'r Run<'p>,
    constants: BTreeMap<String, Vec<usize>>,
    lookups: BTreeMap<(usize, String, String), Vec<usize>>,
}

const MAX_RESOLVE_DEPTH: usize = 4;

impl<'r, 'p> Resolver<'r, 'p> {
    fn new(run: &'r Run<'p>) -> Self {
        let mut constants: BTreeMap<String, Vec<usize>> = BTreeMap::new();
        let mut lookups: BTreeMap<(usize, String, String), Vec<usize>> = BTreeMap::new();
        for (index, entry) in run.bindings.iter().enumerate() {
            let Some(binding) = run
                .packs
                .get(entry.pack)
                .and_then(|p| p.bindings.get(entry.binding))
            else {
                continue;
            };
            if binding.constant {
                constants.entry(entry.name.clone()).or_default().push(index);
            }
            lookups
                .entry((entry.pack, binding.id.clone(), entry.name.clone()))
                .or_default()
                .push(index);
        }
        Self {
            run,
            constants,
            lookups,
        }
    }

    fn path_vars(&self, file: usize, vars: &mut BTreeMap<String, Vec<String>>) {
        let Some(facts) = self.run.files.get(file) else {
            return;
        };
        let path = &facts.path;
        let name = path.file_name().to_owned();
        let stem = name
            .rsplit_once('.')
            .map_or(name.as_str(), |(s, _)| s)
            .to_owned();
        let dir = path
            .parent()
            .map(|p| p.file_name().to_owned())
            .unwrap_or_default();
        vars.insert("path.name".to_owned(), vec![name]);
        vars.insert("path.stem".to_owned(), vec![stem]);
        vars.insert("path.dir".to_owned(), vec![dir]);
    }

    fn capture_vars(
        &self,
        captures: &Captures,
        file: usize,
        depth: usize,
    ) -> BTreeMap<String, Vec<String>> {
        let mut vars = BTreeMap::new();
        for (name, values) in captures {
            let mut out: Vec<String> = Vec::new();
            for value in values {
                match &value.expr {
                    Some(pieces) => out.extend(self.render_pieces(pieces, file, depth)),
                    None => out.push(value.plain.clone()),
                }
            }
            out.dedup();
            out.truncate(crate::pack::template::MAX_ALTERNATIVES);
            vars.insert(name.clone(), out);
        }
        self.path_vars(file, &mut vars);
        vars
    }

    fn render_pieces(&self, pieces: &Pieces, file: usize, depth: usize) -> Vec<String> {
        let mut out = Vec::new();
        for alternative in pieces {
            let mut acc: Vec<String> = vec![String::new()];
            for piece in alternative {
                let options: Vec<String> = match piece {
                    Piece::Lit(text) => vec![text.clone()],
                    Piece::Dyn => vec![DYN.to_string()],
                    Piece::Ref(name) => self
                        .constant(name, file, depth + 1)
                        .unwrap_or_else(|| vec![DYN.to_string()]),
                };
                let mut next = Vec::new();
                for prefix in &acc {
                    for option in &options {
                        if next.len() < crate::pack::template::MAX_ALTERNATIVES {
                            next.push(format!("{prefix}{option}"));
                        }
                    }
                }
                acc = next;
            }
            out.extend(acc);
        }
        out.sort();
        out.dedup();
        out
    }

    /// Values of the constant `name` seen from `file`: same-file constants
    /// win; otherwise a constant defined in exactly one file (or with the
    /// same value everywhere). Ambiguous or unknown names stay unresolved.
    fn constant(&self, name: &str, file: usize, depth: usize) -> Option<Vec<String>> {
        if depth > MAX_RESOLVE_DEPTH {
            return None;
        }
        let mut candidates = vec![name.to_owned()];
        let last = last_segment(name);
        if last != name {
            candidates.push(last.to_owned());
        }
        for candidate in candidates {
            let Some(entries) = self.constants.get(&candidate) else {
                continue;
            };
            let same_file: Vec<usize> = entries
                .iter()
                .copied()
                .filter(|i| self.run.bindings.get(*i).is_some_and(|e| e.file == file))
                .collect();
            if !same_file.is_empty() {
                let values = self.values_of(&same_file, depth);
                if !values.is_empty() {
                    return Some(values);
                }
                continue;
            }
            let mut per_file: BTreeMap<usize, Vec<usize>> = BTreeMap::new();
            for index in entries {
                if let Some(entry) = self.run.bindings.get(*index) {
                    per_file.entry(entry.file).or_default().push(*index);
                }
            }
            let resolved: BTreeSet<Vec<String>> = per_file
                .values()
                .map(|indices| self.values_of(indices, depth))
                .collect();
            if resolved.len() == 1
                && let Some(values) = resolved.into_iter().next()
                && !values.is_empty()
            {
                return Some(values);
            }
        }
        None
    }

    fn values_of(&self, entries: &[usize], depth: usize) -> Vec<String> {
        let mut out: Vec<String> = entries
            .iter()
            .flat_map(|i| self.binding_values(*i, depth))
            .collect();
        out.sort();
        out.dedup();
        out
    }

    fn binding_values(&self, index: usize, depth: usize) -> Vec<String> {
        let Some(entry) = self.run.bindings.get(index) else {
            return Vec::new();
        };
        let Some(binding) = self
            .run
            .packs
            .get(entry.pack)
            .and_then(|p| p.bindings.get(entry.binding))
        else {
            return Vec::new();
        };
        let vars = self.capture_vars(&entry.captures, entry.file, depth);
        binding.value.render(&vars)
    }

    fn lookup(&self, pending: &Pending, binding_id: &str, name: &str) -> Vec<String> {
        let Some(entries) =
            self.lookups
                .get(&(pending.pack, binding_id.to_owned(), name.to_owned()))
        else {
            return Vec::new();
        };
        let Some(binding) = self
            .run
            .packs
            .get(pending.pack)
            .and_then(|p| p.bindings.iter().find(|b| b.id == binding_id))
        else {
            return Vec::new();
        };
        let visible: Vec<usize> = entries
            .iter()
            .copied()
            .filter(|i| {
                let Some(entry) = self.run.bindings.get(*i) else {
                    return false;
                };
                match binding.scope {
                    BindingScope::Symbol => {
                        entry.file == pending.file && entry.enclosing == pending.enclosing
                    }
                    BindingScope::File => entry.file == pending.file,
                    BindingScope::Project => true,
                }
            })
            .collect();
        let preferred: Vec<usize> = if binding.scope == BindingScope::Project {
            let same: Vec<usize> = visible
                .iter()
                .copied()
                .filter(|i| {
                    self.run
                        .bindings
                        .get(*i)
                        .is_some_and(|e| e.file == pending.file)
                })
                .collect();
            if same.is_empty() { visible } else { same }
        } else {
            visible
        };
        self.values_of(&preferred, 0)
    }

    fn render(&self, project: &Name, pending: &Pending) -> Vec<Extraction> {
        let Some(pack) = self.run.packs.get(pending.pack) else {
            return Vec::new();
        };
        let Some(rule) = pack.rules.get(pending.rule) else {
            return Vec::new();
        };
        let Some(facts) = self.run.files.get(pending.file) else {
            return Vec::new();
        };
        let mut vars = self.capture_vars(&pending.captures, pending.file, 0);
        if let Some(route) = &pending.route {
            vars.insert("route".to_owned(), vec![route.clone()]);
        }
        for (var, lookup) in &rule.lookups {
            let by = match lookup.by.strip_prefix('=') {
                Some(fixed) => fixed.to_owned(),
                None => pending
                    .captures
                    .get(&lookup.by)
                    .and_then(|v| v.first())
                    .map(|v| v.plain.clone())
                    .unwrap_or_default(),
            };
            let values = self.lookup(pending, &lookup.binding, &by);
            if !values.is_empty() {
                vars.insert(var.clone(), values);
            }
        }
        for (var, template) in &rule.defaults {
            let empty = vars
                .get(var)
                .is_none_or(|values| values.iter().all(String::is_empty));
            if empty {
                let rendered = template.render(&vars);
                vars.insert(var.clone(), rendered);
            }
        }
        for var in &rule.require {
            let present = vars
                .get(var)
                .is_some_and(|values| values.iter().any(|v| !v.is_empty()));
            if !present {
                return Vec::new();
            }
        }
        for (var, template) in &rule.where_equal {
            let actual = vars
                .get(var)
                .and_then(|v| v.first())
                .cloned()
                .unwrap_or_default();
            let expected = template
                .render(&vars)
                .into_iter()
                .next()
                .unwrap_or_default();
            if actual != expected {
                return Vec::new();
            }
        }
        for (var, matcher) in &rule.unless {
            if vars
                .get(var)
                .is_some_and(|values| values.iter().any(|v| matcher.is_match(v)))
            {
                return Vec::new();
            }
        }

        let mut attrs = BTreeMap::new();
        for (name, template) in &rule.attrs {
            if let Some(value) = template.render(&vars).into_iter().next()
                && !value.is_empty()
            {
                attrs.insert(name.clone(), render_dynamic(&value));
            }
        }
        if rule.glob_all {
            attrs.insert(ATTR_GLOB.to_owned(), "all".to_owned());
        }
        let default_method = if rule.role.is_provider(rule.kind) || rule.role == Role::Definition {
            "*"
        } else {
            "GET"
        };

        let mut out = Vec::new();
        for raw in rule.key.render(&vars) {
            let raw = apply_tokens(&rule.tokens, &raw, &vars);
            let keyed: Vec<(Normalized, Role)> = match rule.postprocess {
                Some(PostProcess::Sql) => sql::tables(&raw)
                    .into_iter()
                    .filter_map(|(table, role)| {
                        normalize(rule.normalizer, &table, default_method).map(|n| (n, role))
                    })
                    .collect(),
                None => normalize(rule.normalizer, &raw, default_method)
                    .map(|n| (n, rule.role))
                    .into_iter()
                    .collect(),
            };
            for (normalized, role) in keyed {
                if !shape_ok(rule, &normalized) {
                    continue;
                }
                let mut attrs = attrs.clone();
                if let Some(host) = &normalized.host {
                    attrs.insert(ATTR_HOST.to_owned(), host.clone());
                }
                if normalized.unresolved {
                    attrs.insert(ATTR_UNRESOLVED_KEY.to_owned(), "true".to_owned());
                }
                out.push(Extraction {
                    project: project.clone(),
                    path: facts.path.clone(),
                    content_hash: facts.content_hash,
                    range: pending.range,
                    kind: rule.kind,
                    role,
                    key: normalized.key,
                    dynamic: normalized.dynamic,
                    symbol: pending.symbol.clone(),
                    evidence: rule.evidence,
                    pack: pack.id(),
                    rule: rule.id.clone(),
                    attrs,
                });
            }
        }
        out
    }
}

fn apply_tokens(
    tokens: &[(String, Template)],
    raw: &str,
    vars: &BTreeMap<String, Vec<String>>,
) -> String {
    let mut out = raw.to_owned();
    for (token, template) in tokens {
        if out.contains(token.as_str()) {
            let value = template.render(vars).into_iter().next().unwrap_or_default();
            out = out.replace(token.as_str(), &value);
        }
    }
    out
}

fn shape_ok(rule: &Rule, normalized: &Normalized) -> bool {
    let key = normalized.key.as_str();
    match rule.key_shape {
        None => !(rule.kind == ContractKind::Endpoint && key.is_empty()),
        Some(KeyShape::Dotted) => {
            !key.chars().any(char::is_whitespace)
                && key
                    .replace("{}", "")
                    .chars()
                    .any(|c| matches!(c, '.' | '-' | '_' | ':' | '/'))
                && !normalized.unresolved
        }
        Some(KeyShape::UpperSnake) => {
            key.chars().next().is_some_and(|c| c.is_ascii_uppercase())
                && key
                    .chars()
                    .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_')
        }
    }
}
