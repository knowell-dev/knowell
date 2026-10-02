//! Query-driven symbol and import extraction, and the language-independent
//! assembly of symbols (containers, qualified names, signatures,
//! visibility).

use std::collections::{HashMap, HashSet, VecDeque};
use std::ops::{ControlFlow, Range};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Instant;

use streaming_iterator::StreamingIterator;
use tree_sitter::{Node, Query, QueryCursor, QueryCursorOptions, QueryCursorState, Tree};

use crate::language::Language;
use crate::model::{Degradation, Import, Symbol, SymbolKind, Visibility};
use crate::text::{
    LineIndex, bounded, indent_before, is_upper_case, one_line, slice, starts_line, strip_quotes,
    trim_end_offset,
};

/// In-progress query states; bounds memory on pathological trees.
const MATCH_LIMIT: u32 = 4096;
const MAX_NAME_BYTES: usize = 256;
const MAX_DOC_BYTES: usize = 2000;
const MAX_SIGNATURE_BYTES: usize = 600;
const MAX_SIGNATURE_LINES: usize = 12;
const MAX_SPECIFIER_BYTES: usize = 512;

/// Wall-clock deadline plus optional caller cancellation.
#[derive(Clone, Copy)]
pub(crate) struct Budget<'a> {
    pub(crate) deadline: Instant,
    pub(crate) cancel: Option<&'a AtomicBool>,
}

impl Budget<'_> {
    /// Why work must stop now, if it must.
    pub(crate) fn exceeded(&self) -> Option<Degradation> {
        if self.cancel.is_some_and(|flag| flag.load(Ordering::Relaxed)) {
            Some(Degradation::Cancelled)
        } else if Instant::now() >= self.deadline {
            Some(Degradation::Timeout)
        } else {
            None
        }
    }

    fn flow(&self) -> ControlFlow<()> {
        if self.exceeded().is_some() {
            ControlFlow::Break(())
        } else {
            ControlFlow::Continue(())
        }
    }
}

/// A symbol before containers and qualified names are known.
#[derive(Debug, Clone)]
pub(crate) struct Draft {
    pub(crate) kind: SymbolKind,
    pub(crate) name: String,
    /// Explicit container prefix (Go receiver, C++ qualifier, SQL table).
    pub(crate) receiver: Option<String>,
    /// Whole declaration including leading docs / attributes.
    pub(crate) range: Range<usize>,
    /// Where the signature starts (after doc comments).
    pub(crate) decl_start: usize,
    pub(crate) name_start: usize,
    /// Where the body starts, when the declaration has one.
    pub(crate) body_start: Option<usize>,
    /// End of the declaration node itself (a file-scoped namespace's range
    /// is later extended over the declarations it scopes).
    pub(crate) decl_end: usize,
    pub(crate) doc: Option<String>,
    /// Declared through a JavaScript / TypeScript `export`.
    pub(crate) exported: bool,
    /// Precomputed signature (structure files, Markdown, Dockerfile).
    pub(crate) signature: Option<String>,
}

impl Draft {
    /// A draft whose signature is given rather than cut from the source.
    pub(crate) fn simple(
        kind: SymbolKind,
        name: String,
        range: Range<usize>,
        signature: String,
    ) -> Self {
        Self {
            kind,
            name,
            receiver: None,
            decl_start: range.start,
            name_start: range.start,
            body_start: None,
            decl_end: range.end,
            doc: None,
            exported: false,
            signature: Some(signature),
            range,
        }
    }
}

/// Per-language syntax facts used during extraction.
struct Rules {
    /// Parent node kinds that wrap a declaration (export, decorators, …).
    wrappers: &'static [&'static str],
    /// Preceding sibling kinds that belong to the declaration.
    attributes: &'static [&'static str],
}

fn rules(language: Language) -> Rules {
    match language {
        Language::Rust => Rules {
            wrappers: &[],
            attributes: &["attribute_item"],
        },
        Language::TypeScript | Language::Tsx | Language::JavaScript | Language::Jsx => Rules {
            wrappers: &[
                "export_statement",
                "lexical_declaration",
                "variable_declaration",
                "ambient_declaration",
                "expression_statement",
            ],
            attributes: &[],
        },
        Language::Python => Rules {
            wrappers: &["decorated_definition", "expression_statement"],
            attributes: &[],
        },
        Language::Go => Rules {
            wrappers: &["type_declaration", "const_declaration", "var_declaration"],
            attributes: &[],
        },
        Language::Dart => Rules {
            wrappers: &["class_member"],
            attributes: &["annotation"],
        },
        Language::Cpp => Rules {
            wrappers: &["template_declaration"],
            attributes: &[],
        },
        Language::CSharp => Rules {
            wrappers: &[],
            attributes: &["attribute_list"],
        },
        Language::Swift | Language::Kotlin | Language::Java => Rules {
            wrappers: &[],
            attributes: &[],
        },
        _ => Rules {
            wrappers: &[],
            attributes: &[],
        },
    }
}

struct RawDef<'t> {
    pattern: usize,
    kind: SymbolKind,
    node: Node<'t>,
    name: Option<Node<'t>>,
    receiver: Option<Node<'t>>,
    body: Option<Node<'t>>,
}

/// Runs the symbols query and returns drafts, plus a degradation if the
/// budget ran out or the match limit was hit.
pub(crate) fn query_symbols(
    tree: &Tree,
    text: &str,
    query: &Query,
    language: Language,
    budget: Budget<'_>,
    max_symbols: usize,
) -> (Vec<Draft>, Option<Degradation>) {
    let mut defs: HashMap<usize, RawDef<'_>> = HashMap::new();
    let names = query.capture_names();
    let mut cursor = QueryCursor::new();
    cursor.set_match_limit(MATCH_LIMIT);
    let mut progress = |_: &QueryCursorState| budget.flow();
    let options = QueryCursorOptions::new().progress_callback(&mut progress);
    let mut degraded = None;
    {
        let mut matches =
            cursor.matches_with_options(query, tree.root_node(), text.as_bytes(), options);
        while let Some(m) = matches.next() {
            let mut def = None;
            let (mut name, mut receiver, mut body) = (None, None, None);
            for capture in m.captures() {
                let capture_name = names.get(capture.index as usize).copied().unwrap_or("");
                match capture_name {
                    "name" => name = Some(capture.node),
                    "receiver" => receiver = Some(capture.node),
                    "body" => body = Some(capture.node),
                    other => {
                        if let Some(kind) = other
                            .strip_prefix("definition.")
                            .and_then(SymbolKind::from_capture)
                        {
                            def = Some((kind, capture.node));
                        }
                    }
                }
            }
            let Some((kind, node)) = def else { continue };
            let candidate = RawDef {
                pattern: m.pattern_index,
                kind,
                node,
                name,
                receiver,
                body,
            };
            match defs.get(&node.id()) {
                Some(existing) if existing.pattern <= candidate.pattern => {}
                _ => {
                    defs.insert(node.id(), candidate);
                }
            }
            if defs.len() > max_symbols {
                degraded = Some(Degradation::Truncated { limit: max_symbols });
                break;
            }
        }
    }
    if degraded.is_none() {
        degraded = budget.exceeded();
    }
    if degraded.is_none() && cursor.did_exceed_match_limit() {
        degraded = Some(Degradation::Truncated {
            limit: MATCH_LIMIT as usize,
        });
    }

    let mut raws: Vec<RawDef<'_>> = defs.into_values().collect();
    raws.sort_by_key(|raw| {
        (
            raw.node.start_byte(),
            std::cmp::Reverse(raw.node.end_byte()),
            raw.pattern,
        )
    });
    let rules = rules(language);
    let targets: HashSet<usize> = raws.iter().map(|raw| raw.node.id()).collect();
    let Some(contexts) = contexts(tree, &targets, &rules, budget) else {
        return (Vec::new(), budget.exceeded().or(Some(Degradation::Timeout)));
    };
    let drafts = drafts_from_raw(&raws, &contexts, text, language, &rules);
    (drafts, degraded)
}

/// Ancestors kept per declaration (wrappers are at most 4 levels up).
const MAX_ANCESTORS: usize = 6;
/// Comment / attribute nodes kept directly before a node.
const MAX_LEAD_RUN: usize = 256;

/// The trivia (comments, attributes) directly before a node.
struct Lead<'t> {
    /// Adjacent trivia siblings, in source order (at most `MAX_LEAD_RUN`).
    run: Vec<Node<'t>>,
    /// A non-trivia sibling precedes the run.
    preceded: bool,
}

/// Where a declaration node sits: its ancestors and the trivia before it
/// and before each ancestor.
struct Context<'t> {
    /// Nearest first.
    ancestors: Vec<Node<'t>>,
    /// `leads[0]` precedes the node, `leads[i]` precedes `ancestors[i - 1]`.
    leads: Vec<Lead<'t>>,
}

fn is_trivia(node: Node<'_>, rules: &Rules) -> bool {
    node.kind().contains("comment") || rules.attributes.contains(&node.kind())
}

/// Collects the [`Context`] of every target node in one cursor walk.
///
/// tree-sitter's `parent` and `prev_sibling` are linear in the number of
/// siblings, so calling them per declaration is quadratic on flat files
/// (thousands of `#define`s or constants); this walk is linear. Returns
/// `None` when the budget runs out.
fn contexts<'t>(
    tree: &'t Tree,
    targets: &HashSet<usize>,
    rules: &Rules,
    budget: Budget<'_>,
) -> Option<HashMap<usize, Context<'t>>> {
    struct Frame<'t> {
        node: Node<'t>,
        run: VecDeque<Node<'t>>,
        preceded: bool,
    }
    let mut found = HashMap::with_capacity(targets.len());
    let mut frames: Vec<Frame<'t>> = Vec::new();
    let mut cursor = tree.walk();
    let mut visited: usize = 0;
    loop {
        visited += 1;
        if visited.is_multiple_of(4096) && budget.exceeded().is_some() {
            return None;
        }
        let node = cursor.node();
        if targets.contains(&node.id()) {
            let levels = frames.iter().rev().take(MAX_ANCESTORS + 1);
            let leads = levels
                .clone()
                .map(|frame| Lead {
                    run: frame.run.iter().copied().collect(),
                    preceded: frame.preceded,
                })
                .collect();
            let ancestors = levels.take(MAX_ANCESTORS).map(|frame| frame.node).collect();
            found.insert(node.id(), Context { ancestors, leads });
        }
        if cursor.goto_first_child() {
            frames.push(Frame {
                node,
                run: VecDeque::new(),
                preceded: false,
            });
            continue;
        }
        // Done with this node: record it in its parent's frame, then move to
        // the next sibling, climbing as far as needed.
        loop {
            let done = cursor.node();
            if let Some(frame) = frames.last_mut() {
                if is_trivia(done, rules) {
                    if frame.run.len() == MAX_LEAD_RUN {
                        frame.run.pop_front();
                    }
                    frame.run.push_back(done);
                } else {
                    frame.run.clear();
                    frame.preceded = true;
                }
            }
            if cursor.goto_next_sibling() {
                break;
            }
            if !cursor.goto_parent() {
                return Some(found);
            }
            frames.pop();
        }
    }
}

fn drafts_from_raw(
    raws: &[RawDef<'_>],
    contexts: &HashMap<usize, Context<'_>>,
    text: &str,
    language: Language,
    rules: &Rules,
) -> Vec<Draft> {
    // Climb wrappers; a wrapper shared by several declarations
    // (`const a = () => 1, b = () => 2`) is not climbed.
    let outers: Vec<(Node<'_>, usize, bool)> = raws
        .iter()
        .map(|raw| climb(raw.node, contexts.get(&raw.node.id()), rules))
        .collect();
    let mut shared: HashMap<usize, usize> = HashMap::new();
    for (outer, _, _) in &outers {
        *shared.entry(outer.id()).or_insert(0) += 1;
    }
    raws.iter()
        .zip(outers)
        .filter_map(|(raw, (outer, level, exported))| {
            let unique = shared.get(&outer.id()).copied().unwrap_or(0) <= 1;
            let (outer, level, exported) = if unique {
                (outer, level, exported)
            } else {
                (raw.node, 0, false)
            };
            let context = contexts.get(&raw.node.id());
            draft(raw, outer, level, context, exported, text, language)
        })
        .collect()
}

/// Climbs wrapper ancestors (`export`, decorators, …). Returns the outer
/// node, how many levels were climbed, and whether an `export` was crossed.
fn climb<'t>(
    node: Node<'t>,
    context: Option<&Context<'t>>,
    rules: &Rules,
) -> (Node<'t>, usize, bool) {
    let mut current = node;
    let mut level = 0;
    let mut exported = false;
    if let Some(context) = context {
        for ancestor in context.ancestors.iter().take(4) {
            if !rules.wrappers.contains(&ancestor.kind()) {
                break;
            }
            if ancestor.kind() == "export_statement" {
                exported = true;
            }
            current = *ancestor;
            level += 1;
        }
    }
    (current, level, exported)
}

fn draft(
    raw: &RawDef<'_>,
    outer: Node<'_>,
    level: usize,
    context: Option<&Context<'_>>,
    exported: bool,
    text: &str,
    language: Language,
) -> Option<Draft> {
    let (lead_start, mut doc) = leading(outer, level, context, text, language);
    if language == Language::Python
        && let Some(docstring) = python_docstring(raw.node, text)
    {
        doc = Some(docstring);
    }
    let decl_start = outer.start_byte();
    let end = trim_end_offset(text, decl_start, outer.end_byte().max(raw.node.end_byte()));
    let body_start = raw
        .body
        .map(|body| body.start_byte())
        .filter(|&b| b >= decl_start && b <= end);

    let (receiver, name, name_start) = match raw.name {
        Some(name_node) => {
            let (receiver, name) = split_name(slice(text, &name_node.byte_range()), language);
            (receiver, name, name_node.start_byte())
        }
        None => {
            // Unnamed at-rules and the like are named by their prelude.
            let cut = body_start.unwrap_or(end);
            let prelude = one_line(slice(text, &(decl_start..cut)));
            (None, bounded(&prelude, 120), decl_start)
        }
    };
    if name.is_empty() {
        return None;
    }
    let receiver = raw
        .receiver
        .map(|node| clean_type_name(slice(text, &node.byte_range())))
        .filter(|r| !r.is_empty())
        .or(receiver);
    Some(Draft {
        kind: raw.kind,
        name,
        receiver,
        range: lead_start..end,
        decl_start,
        name_start,
        body_start,
        decl_end: end,
        doc,
        exported,
        signature: None,
    })
}

/// Splits `Outer::name` (C++) into receiver and name; strips quotes and
/// generic arguments; bounds the length.
fn split_name(raw: &str, language: Language) -> (Option<String>, String) {
    let raw = one_line(strip_quotes(raw));
    if matches!(language, Language::Cpp | Language::C)
        && let Some((scope, name)) = raw.rsplit_once("::")
    {
        let receiver = clean_type_name(scope);
        return (
            (!receiver.is_empty()).then_some(receiver),
            bounded(name.trim(), MAX_NAME_BYTES),
        );
    }
    (None, bounded(&raw, MAX_NAME_BYTES))
}

/// `Foo<T>` / `*Foo` / `&'a Foo` / `pkg::Foo` → `Foo`.
pub(crate) fn clean_type_name(raw: &str) -> String {
    let raw = one_line(raw);
    let without_generics = raw.split(['<', '[']).next().unwrap_or("");
    let trimmed = without_generics
        .trim()
        .trim_start_matches(['*', '&'])
        .trim_start_matches("mut ")
        .trim();
    let last = trimmed.rsplit("::").next().unwrap_or(trimmed);
    bounded(last.trim(), MAX_NAME_BYTES)
}

fn end_row(node: Node<'_>) -> usize {
    let end = node.end_position();
    if end.column == 0 && end.row > node.start_position().row {
        end.row - 1
    } else {
        end.row
    }
}

/// Extends a declaration upwards over adjacent comments and attributes.
/// Returns the new start and the doc text found in those comments.
///
/// `level` is how many wrapper levels `outer` is above the declaration node
/// (its lead is `context.leads[level]`).
fn leading(
    outer: Node<'_>,
    level: usize,
    context: Option<&Context<'_>>,
    text: &str,
    language: Language,
) -> (usize, Option<String>) {
    let mut start = outer.start_byte();
    let mut start_row = outer.start_position().row;
    let mut docs: Vec<&str> = Vec::new();
    let Some(context) = context else {
        return (start, None);
    };
    let mut level = level;
    'levels: while let Some(lead) = context.leads.get(level) {
        for prev in lead.run.iter().rev() {
            if end_row(*prev) + 1 < start_row {
                break 'levels; // a blank line separates it
            }
            if prev.kind().contains("comment") {
                if !starts_line(text, prev.start_byte()) {
                    break 'levels; // trailing comment of the previous statement
                }
                let raw = slice(text, &prev.byte_range());
                if raw.starts_with("#!") {
                    break 'levels;
                }
                if is_doc_comment(raw, language) {
                    docs.push(raw);
                }
            }
            start = prev.start_byte();
            start_row = prev.start_position().row;
        }
        // Nothing but trivia before it: when the parent starts at the same
        // place (a Ruby `class` opening a `body_statement`), the comments
        // are siblings of the parent.
        let parent_starts_here = context
            .ancestors
            .get(level)
            .is_some_and(|parent| parent.start_byte() == start);
        if lead.preceded || !parent_starts_here {
            break;
        }
        level += 1;
    }
    docs.reverse();
    (start, clean_doc(&docs, language))
}

fn is_doc_comment(raw: &str, language: Language) -> bool {
    match language {
        Language::Rust => {
            (raw.starts_with("///") && !raw.starts_with("////"))
                || (raw.starts_with("/**") && !raw.starts_with("/***") && raw != "/**/")
        }
        // Python docs are docstrings; `#` comments above are only absorbed.
        Language::Python => false,
        _ => true,
    }
}

/// Comment directives that are not documentation.
const DIRECTIVES: &[&str] = &[
    "go:",
    "+build",
    "eslint-",
    "@ts-",
    "prettier-ignore",
    "noqa",
    "type:",
    "pylint:",
    "nolint",
    "rubocop:",
    "istanbul ",
    "c8 ",
    "NOSONAR",
    "clang-format",
    "region",
    "endregion",
];

/// Strips comment markers from adjacent comment nodes and joins them.
pub(crate) fn clean_doc(comments: &[&str], language: Language) -> Option<String> {
    let mut lines: Vec<String> = Vec::new();
    for raw in comments {
        let raw = raw.trim();
        if let Some(block) = raw.strip_prefix("/*") {
            let block = block.strip_suffix("*/").unwrap_or(block);
            let block = block.strip_prefix(['*', '!']).unwrap_or(block);
            for line in block.lines() {
                let line = line.trim();
                let line = line.strip_prefix('*').unwrap_or(line);
                lines.push(line.strip_prefix(' ').unwrap_or(line).trim_end().to_owned());
            }
        } else {
            for line in raw.lines() {
                let line = line.trim();
                let stripped = ["///", "//!", "//", "#", "--", ";;"]
                    .iter()
                    .find_map(|marker| line.strip_prefix(marker))
                    .unwrap_or(line);
                let stripped = stripped.strip_prefix(['/', '!']).unwrap_or(stripped);
                let stripped = stripped.strip_prefix(' ').unwrap_or(stripped).trim_end();
                if DIRECTIVES.iter().any(|d| stripped.starts_with(d)) {
                    continue;
                }
                lines.push(stripped.to_owned());
            }
        }
    }
    let mut text = lines.join("\n");
    if language == Language::CSharp {
        text = strip_xml_tags(&text);
    }
    finish_doc(&text)
}

fn finish_doc(text: &str) -> Option<String> {
    // Collapse runs of blank lines and trim.
    let mut out = String::new();
    let mut blank = false;
    for line in text.lines() {
        let line = line.trim_end();
        if line.trim().is_empty() {
            blank = !out.is_empty();
            continue;
        }
        if blank {
            out.push_str("\n\n");
        } else if !out.is_empty() {
            out.push('\n');
        }
        blank = false;
        out.push_str(line);
    }
    (!out.is_empty()).then(|| bounded(&out, MAX_DOC_BYTES))
}

fn strip_xml_tags(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut in_tag = false;
    for c in text.chars() {
        match c {
            '<' => in_tag = true,
            '>' if in_tag => in_tag = false,
            _ if !in_tag => out.push(c),
            _ => {}
        }
    }
    out
}

fn python_docstring(def: Node<'_>, text: &str) -> Option<String> {
    let body = def.child_by_field_name("body")?;
    let first = body.named_child(0)?;
    if first.kind() != "expression_statement" {
        return None;
    }
    let string = first.named_child(0)?;
    if string.kind() != "string" {
        return None;
    }
    clean_docstring(slice(text, &string.byte_range()))
}

/// Removes string prefixes and quotes and dedents (like `inspect.cleandoc`).
pub(crate) fn clean_docstring(raw: &str) -> Option<String> {
    let raw = raw.trim_start_matches(['r', 'R', 'u', 'U', 'b', 'B', 'f', 'F']);
    let inner = ["\"\"\"", "'''", "\"", "'"]
        .iter()
        .find_map(|q| raw.strip_prefix(q).and_then(|r| r.strip_suffix(q)))
        .unwrap_or(raw);
    let mut lines = inner.lines();
    let first = lines.next().unwrap_or("").trim().to_owned();
    let rest: Vec<&str> = lines.collect();
    let indent = rest
        .iter()
        .filter(|l| !l.trim().is_empty())
        .map(|l| l.len() - l.trim_start().len())
        .min()
        .unwrap_or(0);
    let mut out = vec![first];
    out.extend(rest.iter().map(|l| {
        l.get(indent..)
            .unwrap_or(l.trim_start())
            .trim_end()
            .to_owned()
    }));
    finish_doc(&out.join("\n"))
}

/// Assembles drafts into symbols: containers, qualified names, kind fixes,
/// signatures and visibility. Output is sorted by start offset.
pub(crate) fn finish(
    mut drafts: Vec<Draft>,
    text: &str,
    lines: &LineIndex,
    language: Language,
) -> Vec<Symbol> {
    for draft in &mut drafts {
        let end = draft.range.end.min(text.len());
        let start = draft.range.start.min(end);
        let end = trim_end_offset(text, start, end).max(start);
        draft.range = start..end;
        draft.decl_start = draft.decl_start.clamp(start, end);
        draft.name_start = draft.name_start.clamp(start, end);
    }
    drafts.sort_by(|a, b| {
        a.range
            .start
            .cmp(&b.range.start)
            .then(b.range.end.cmp(&a.range.end))
            .then(a.decl_start.cmp(&b.decl_start))
            .then(a.kind.cmp(&b.kind))
            .then(a.name.cmp(&b.name))
    });
    drafts.dedup_by(|a, b| a.range == b.range && a.kind == b.kind && a.name == b.name);
    let line_ranges: Vec<_> = drafts
        .iter()
        .map(|draft| lines.range(&draft.range))
        .collect();
    let drafts: Vec<(Draft, knowell_core::LineRange)> = drafts
        .into_iter()
        .zip(line_ranges)
        .filter_map(|(draft, range)| range.map(|r| (draft, r)))
        .collect();

    // Containment: a stack of open containers over start-sorted ranges.
    let mut parents: Vec<Option<usize>> = Vec::with_capacity(drafts.len());
    let mut stack: Vec<usize> = Vec::new();
    for (index, (draft, _)) in drafts.iter().enumerate() {
        while let Some(&top) = stack.last() {
            let contains = drafts.get(top).is_some_and(|(outer, _)| {
                outer.range.start <= draft.range.start && draft.range.end <= outer.range.end
            });
            if contains {
                break;
            }
            stack.pop();
        }
        parents.push(stack.last().copied());
        if draft.kind.is_container() {
            stack.push(index);
        }
    }

    // Kind fixes depend on the container.
    let kinds: Vec<SymbolKind> = drafts
        .iter()
        .zip(&parents)
        .map(|((draft, _), parent)| {
            let parent = parent.and_then(|p| drafts.get(p)).map(|(d, _)| d);
            fix_kind(draft, parent, language)
        })
        .collect();

    // Qualified names (parents precede children).
    let mut qualified: Vec<String> = Vec::with_capacity(drafts.len());
    for (((draft, _), parent), kind) in drafts.iter().zip(&parents).zip(&kinds) {
        let segment = match &draft.receiver {
            Some(receiver) => format!("{receiver}.{}", draft.name),
            None => draft.name.clone(),
        };
        let name = match parent.and_then(|p| qualified.get(p)) {
            Some(prefix) => format!("{prefix}{}{segment}", kind.separator()),
            None => segment,
        };
        qualified.push(name);
    }

    let mut first_child: Vec<Option<usize>> = vec![None; drafts.len()];
    for ((draft, _), parent) in drafts.iter().zip(&parents) {
        if let Some(slot) = parent.and_then(|p| first_child.get_mut(p))
            && slot.is_none_or(|s| draft.range.start < s)
        {
            *slot = Some(draft.range.start);
        }
    }

    let mut symbols = Vec::with_capacity(drafts.len());
    for (index, (draft, line_range)) in drafts.iter().enumerate() {
        let parent = parents.get(index).copied().flatten();
        let kind = kinds.get(index).copied().unwrap_or(draft.kind);
        let has_body = draft.body_start.is_some();
        let signature = match &draft.signature {
            Some(signature) => signature.clone(),
            None => {
                // The signature ends where the body, the first member (with
                // its leading comments) or the declaration itself starts /
                // ends, whichever comes first.
                let cut = [
                    draft.body_start,
                    first_child.get(index).copied().flatten(),
                    Some(draft.decl_end),
                ]
                .into_iter()
                .flatten()
                .min()
                .unwrap_or(draft.range.end)
                .clamp(draft.decl_start, draft.range.end);
                normalize_signature(text, draft.decl_start, cut, language)
            }
        };
        let parent_info = parent.and_then(|p| {
            let (d, _) = drafts.get(p)?;
            Some((kinds.get(p).copied().unwrap_or(d.kind), d))
        });
        let visibility = visibility(language, draft, kind, text, parent_info);
        symbols.push(Symbol {
            name: draft.name.clone(),
            qualified_name: qualified.get(index).cloned().unwrap_or_default(),
            kind,
            range: *line_range,
            byte_range: draft.range.clone(),
            name_line: lines.line_of(draft.name_start),
            signature,
            doc: draft.doc.clone(),
            visibility,
            parent,
            has_body,
        });
    }
    symbols
}

fn fix_kind(draft: &Draft, parent: Option<&Draft>, language: Language) -> SymbolKind {
    let parent_kind = parent.map(|p| p.kind);
    let mut kind = draft.kind;
    if kind == SymbolKind::Function
        && parent_kind.is_some_and(|k| {
            k.is_class_like() || (language == Language::Ruby && k == SymbolKind::Module)
        })
    {
        kind = SymbolKind::Method;
    }
    if matches!(kind, SymbolKind::Method | SymbolKind::Function) {
        let constructor = match language {
            Language::TypeScript | Language::Tsx | Language::JavaScript | Language::Jsx => {
                draft.name == "constructor"
            }
            Language::Python => draft.name == "__init__",
            Language::Php => draft.name == "__construct",
            Language::Ruby => draft.name == "initialize",
            Language::Cpp => parent.is_some_and(|p| {
                matches!(p.kind, SymbolKind::Class | SymbolKind::Struct) && p.name == draft.name
            }),
            _ => false,
        };
        if constructor && kind == SymbolKind::Method {
            kind = SymbolKind::Constructor;
        }
    }
    if kind == SymbolKind::Variable && is_upper_case(&draft.name) {
        kind = SymbolKind::Constant;
    }
    kind
}

/// Cuts `text[start..cut]`, removes the trailing body opener and normalises
/// whitespace while keeping relative indentation of continuation lines.
pub(crate) fn normalize_signature(
    text: &str,
    start: usize,
    cut: usize,
    language: Language,
) -> String {
    let indent = indent_before(text, start);
    let raw = slice(text, &(start..cut));
    let mut lines: Vec<&str> = Vec::new();
    for (i, line) in raw.lines().enumerate() {
        let line = line.trim_end();
        if i == 0 {
            lines.push(line);
        } else {
            lines.push(line.strip_prefix(indent).unwrap_or(line.trim_start()));
        }
    }
    while lines.last().is_some_and(|l| l.trim().is_empty()) {
        lines.pop();
    }
    let truncated_lines = lines.len() > MAX_SIGNATURE_LINES;
    lines.truncate(MAX_SIGNATURE_LINES);
    let mut signature = lines.join("\n");
    // Drop a trailing body opener: `{`, Python `:`, Scala / Kotlin `=`.
    // Arrow functions keep their `=>`.
    for _ in 0..2 {
        let trimmed = signature.trim_end();
        let next = if let Some(rest) = trimmed.strip_suffix('{') {
            rest
        } else if language == Language::Python && trimmed.ends_with(':') {
            trimmed.strip_suffix(':').unwrap_or(trimmed)
        } else if trimmed.ends_with('=') && !trimmed.ends_with("==") && !trimmed.ends_with("=>") {
            trimmed.strip_suffix('=').unwrap_or(trimmed)
        } else {
            break;
        };
        signature = next.trim_end().to_owned();
    }
    let mut signature = bounded(signature.trim_end(), MAX_SIGNATURE_BYTES);
    if truncated_lines && !signature.ends_with('…') {
        signature.push_str(" …");
    }
    signature
}

fn has_word(text: &str, word: &str) -> bool {
    text.split(|c: char| !(c.is_alphanumeric() || c == '_'))
        .any(|w| w == word)
}

fn visibility(
    language: Language,
    draft: &Draft,
    kind: SymbolKind,
    text: &str,
    parent: Option<(SymbolKind, &Draft)>,
) -> Option<Visibility> {
    let prefix = slice(text, &(draft.decl_start..draft.name_start));
    let name = draft.name.as_str();
    let parent_kind = parent.map(|(k, _)| k);
    match language {
        Language::Rust => {
            if kind == SymbolKind::Impl {
                return None;
            }
            if prefix.contains("pub(self)") {
                Some(Visibility::Private)
            } else if prefix.contains("pub(crate)")
                || prefix.contains("pub(super)")
                || prefix.contains("pub(in")
            {
                Some(Visibility::Internal)
            } else if has_word(prefix, "pub") {
                Some(Visibility::Public)
            } else if parent.is_some_and(|(k, p)| {
                k == SymbolKind::Trait
                    || (k == SymbolKind::Impl && has_word(&rust_impl_header(text, p), "for"))
            }) {
                // Trait items and trait-impl items are as visible as the trait.
                Some(Visibility::Public)
            } else {
                Some(Visibility::Private)
            }
        }
        Language::Go => name.chars().next().map(|c| {
            if c.is_uppercase() {
                Visibility::Public
            } else {
                Visibility::Private
            }
        }),
        Language::Python => Some(if name.starts_with("__") && name.ends_with("__") {
            Visibility::Public
        } else if name.starts_with('_') {
            Visibility::Private
        } else {
            Visibility::Public
        }),
        Language::TypeScript | Language::Tsx | Language::JavaScript | Language::Jsx => {
            if kind == SymbolKind::Test {
                None
            } else if name.starts_with('#') || has_word(prefix, "private") {
                Some(Visibility::Private)
            } else if has_word(prefix, "protected") {
                Some(Visibility::Protected)
            } else if has_word(prefix, "public")
                || draft.exported
                || parent_kind.is_some_and(|k| k.is_class_like())
            {
                // Class and interface members default to public.
                Some(Visibility::Public)
            } else if matches!(language, Language::TypeScript | Language::Tsx) {
                // Not exported from an ES module.
                Some(Visibility::Private)
            } else {
                // CommonJS exports are assignments; unknown syntactically.
                None
            }
        }
        Language::Java => {
            keyword_visibility(prefix).or(Some(if parent_kind == Some(SymbolKind::Interface) {
                Visibility::Public
            } else {
                Visibility::Internal
            }))
        }
        Language::Kotlin | Language::Php | Language::Scala => {
            if kind == SymbolKind::Module {
                return None;
            }
            keyword_visibility(prefix).or(Some(Visibility::Public))
        }
        Language::CSharp => {
            if kind == SymbolKind::Module {
                return None;
            }
            keyword_visibility(prefix).or(Some(match parent_kind {
                None | Some(SymbolKind::Module) => Visibility::Internal,
                Some(SymbolKind::Interface) => Visibility::Public,
                Some(_) => Visibility::Private,
            }))
        }
        Language::Swift => {
            if kind == SymbolKind::Impl {
                return None;
            }
            if has_word(prefix, "private") || has_word(prefix, "fileprivate") {
                Some(Visibility::Private)
            } else if has_word(prefix, "public") || has_word(prefix, "open") {
                Some(Visibility::Public)
            } else {
                Some(Visibility::Internal)
            }
        }
        Language::Dart => Some(if name.starts_with('_') {
            Visibility::Private
        } else {
            Visibility::Public
        }),
        Language::C | Language::Cpp => {
            let top_level = parent_kind.is_none_or(|k| k == SymbolKind::Module);
            if kind == SymbolKind::Function && top_level {
                Some(if has_word(prefix, "static") {
                    Visibility::Private
                } else {
                    Visibility::Public
                })
            } else {
                None
            }
        }
        _ => None,
    }
}

fn keyword_visibility(prefix: &str) -> Option<Visibility> {
    if has_word(prefix, "private") {
        Some(Visibility::Private)
    } else if has_word(prefix, "protected") {
        Some(Visibility::Protected)
    } else if has_word(prefix, "internal") {
        Some(Visibility::Internal)
    } else if has_word(prefix, "public") || has_word(prefix, "open") {
        Some(Visibility::Public)
    } else {
        None
    }
}

fn rust_impl_header(text: &str, imp: &Draft) -> String {
    let cut = imp.body_start.unwrap_or(imp.range.end);
    slice(text, &(imp.decl_start..cut)).to_owned()
}

/// Runs the imports query.
pub(crate) fn query_imports(
    tree: &Tree,
    text: &str,
    query: &Query,
    lines: &LineIndex,
    budget: Budget<'_>,
) -> (Vec<Import>, Option<Degradation>) {
    let names = query.capture_names();
    let mut cursor = QueryCursor::new();
    cursor.set_match_limit(MATCH_LIMIT);
    let mut progress = |_: &QueryCursorState| budget.flow();
    let options = QueryCursorOptions::new().progress_callback(&mut progress);
    let mut found: Vec<(usize, Import)> = Vec::new();
    {
        let mut matches =
            cursor.matches_with_options(query, tree.root_node(), text.as_bytes(), options);
        while let Some(m) = matches.next() {
            let mut statement = None;
            let mut source = None;
            for capture in m.captures() {
                match names.get(capture.index as usize).copied().unwrap_or("") {
                    "import" => statement = Some(capture.node),
                    "import.source" => source = Some(capture.node),
                    _ => {}
                }
            }
            let Some(statement) = statement else { continue };
            let specifier = match source {
                Some(source) => one_line(strip_quotes(slice(text, &source.byte_range()))),
                None => specifier_from_statement(slice(text, &statement.byte_range())),
            };
            if specifier.is_empty() {
                continue;
            }
            let Some(range) = lines.range(&statement.byte_range()) else {
                continue;
            };
            found.push((
                statement.start_byte(),
                Import {
                    specifier: bounded(&specifier, MAX_SPECIFIER_BYTES),
                    range,
                },
            ));
        }
    }
    found.sort_by(|a, b| a.0.cmp(&b.0).then(a.1.specifier.cmp(&b.1.specifier)));
    found.dedup_by(|a, b| a.1 == b.1);
    (
        found.into_iter().map(|(_, import)| import).collect(),
        budget.exceeded(),
    )
}

/// Derives the specifier from a whole import statement when the grammar has
/// no dedicated node for it (`import static a.b.C;`, `using X = Y.Z;`).
pub(crate) fn specifier_from_statement(statement: &str) -> String {
    const KEYWORDS: &[&str] = &[
        "global", "using", "import", "static", "use", "function", "const", "export", "from",
        "require", "#include", "@import",
    ];
    let mut text = one_line(statement);
    text = text.trim_end_matches(';').trim().to_owned();
    while let Some((first, rest)) = text.split_once(' ') {
        if !KEYWORDS.contains(&first) {
            break;
        }
        text = rest.trim().to_owned();
    }
    // Python `from x import y` (future imports): keep the module.
    if let Some((module, _)) = text.split_once(" import ") {
        text = module.trim().to_owned();
    }
    // C# aliases: `Alias = Namespace.Type`.
    if let Some((alias, target)) = text.split_once('=')
        && !alias.trim().contains(' ')
        && !alias.contains('{')
    {
        text = target.trim().to_owned();
    }
    strip_quotes(&text).to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn doc_cleaning() {
        assert_eq!(
            clean_doc(&["/// Cancels it.\n", "/// Second line."], Language::Rust).as_deref(),
            Some("Cancels it.\nSecond line.")
        );
        assert_eq!(
            clean_doc(
                &["/**\n * Creates the app.\n *\n * More.\n */"],
                Language::JavaScript
            )
            .as_deref(),
            Some("Creates the app.\n\nMore.")
        );
        assert_eq!(
            clean_doc(&["/// <summary>Cancels it.</summary>"], Language::CSharp).as_deref(),
            Some("Cancels it.")
        );
        assert_eq!(
            clean_doc(&["//go:generate stringer", "// Real doc."], Language::Go).as_deref(),
            Some("Real doc.")
        );
        assert_eq!(clean_doc(&["//"], Language::Go), None);
        assert!(is_doc_comment("/// x", Language::Rust));
        assert!(!is_doc_comment("// x", Language::Rust));
        assert!(!is_doc_comment("//// x", Language::Rust));
    }

    #[test]
    fn docstrings() {
        assert_eq!(
            clean_docstring("\"\"\"Adds things.\n\n    More detail.\n    \"\"\"").as_deref(),
            Some("Adds things.\n\nMore detail.")
        );
        assert_eq!(clean_docstring("r'''x'''").as_deref(), Some("x"));
        assert_eq!(clean_docstring("\"\"\"\"\"\""), None);
    }

    #[test]
    fn statement_specifiers() {
        assert_eq!(
            specifier_from_statement("import static java.util.Objects.requireNonNull;"),
            "java.util.Objects.requireNonNull"
        );
        assert_eq!(
            specifier_from_statement("using static System.Math;"),
            "System.Math"
        );
        assert_eq!(specifier_from_statement("global using System;"), "System");
        assert_eq!(
            specifier_from_statement("using Json = Newtonsoft.Json;"),
            "Newtonsoft.Json"
        );
        assert_eq!(
            specifier_from_statement("import com.example.{Plan, Repo}"),
            "com.example.{Plan, Repo}"
        );
        assert_eq!(
            specifier_from_statement("from __future__ import annotations"),
            "__future__"
        );
        assert_eq!(
            specifier_from_statement("use function App\\helper;"),
            "App\\helper"
        );
    }

    #[test]
    fn names() {
        assert_eq!(clean_type_name("Foo<T, U>"), "Foo");
        assert_eq!(clean_type_name("*Service"), "Service");
        assert_eq!(clean_type_name("crate::a::Foo"), "Foo");
        assert_eq!(
            split_name("billing::Service::cancel", Language::Cpp),
            (Some("Service".to_owned()), "cancel".to_owned())
        );
        assert_eq!(
            split_name("'x'", Language::TypeScript),
            (None, "x".to_owned())
        );
    }

    #[test]
    fn signatures() {
        let text = "    pub fn new(\n        a: u32,\n    ) -> Self {\n";
        assert_eq!(
            normalize_signature(text, 4, text.len(), Language::Rust),
            "pub fn new(\n    a: u32,\n) -> Self"
        );
        assert_eq!(
            normalize_signature("def f(a) -> int:", 0, 16, Language::Python),
            "def f(a) -> int"
        );
        assert_eq!(
            normalize_signature("def apply(x: Int): Int =", 0, 24, Language::Scala),
            "def apply(x: Int): Int"
        );
        assert_eq!(
            normalize_signature("if a == b", 0, 9, Language::Scala),
            "if a == b"
        );
    }
}
