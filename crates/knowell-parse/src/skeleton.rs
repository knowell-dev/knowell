//! Skeletons: signatures, type declarations and doc summaries with bodies
//! elided, for compact context packing.

use crate::ParseError;
use crate::chunk::verify;
use crate::language::{Language, Tier};
use crate::model::{ParsedFile, Symbol, SymbolKind};
use crate::text::{one_line, summary};

/// Lines of doc summary kept per symbol.
const DOC_LINES: usize = 3;
const INDENT: &str = "    ";
/// Deepest indentation level rendered; deeper members stay at this level.
const MAX_INDENT_DEPTH: usize = 32;

/// Renders the file's declarations with bodies elided.
///
/// Code (exact and structural tiers, protobuf): doc summary as comments,
/// signature, then `{ … }` (`...` in Python, `… end` in Ruby) for bodies;
/// containers list their members indented inside braces. Structure files:
/// an indented outline of signatures (`## Setup`, `GET /x (operationId: y)`,
/// `[package]`); SQL tables list their columns. Text-only and degraded files
/// without symbols yield an empty string.
///
/// # Errors
///
/// [`ParseError::TextMismatch`] if `text` is not the text `parsed` was
/// produced from.
pub fn skeleton(parsed: &ParsedFile, text: &str) -> Result<String, ParseError> {
    verify(parsed, text)?;
    let mut children: Vec<Vec<usize>> = vec![Vec::new(); parsed.symbols.len()];
    let mut roots = Vec::new();
    for (index, symbol) in parsed.symbols.iter().enumerate() {
        match symbol.parent.and_then(|p| children.get_mut(p)) {
            Some(list) => list.push(index),
            None => roots.push(index),
        }
    }
    let renderer = Renderer {
        parsed,
        children: &children,
    };
    // An explicit task stack instead of recursion: nesting depth is
    // controlled by the input.
    let mut out = String::new();
    let mut tasks: Vec<Task> = roots
        .into_iter()
        .rev()
        .map(|index| Task::Open { index, depth: 0 })
        .collect();
    while let Some(task) = tasks.pop() {
        match task {
            Task::Open { index, depth } => renderer.open(index, depth, &mut out, &mut tasks),
            Task::Close(line) => out.push_str(&line),
        }
    }
    Ok(out)
}

enum Task {
    /// Render a symbol and queue its members.
    Open { index: usize, depth: usize },
    /// Emit a closing line (`}`, `end`) after the members.
    Close(String),
}

fn indent(depth: usize) -> String {
    INDENT.repeat(depth.min(MAX_INDENT_DEPTH))
}

/// Queues members (in source order) followed by an optional closing line.
fn queue(tasks: &mut Vec<Task>, kids: &[usize], depth: usize, close: Option<String>) {
    if let Some(close) = close {
        tasks.push(Task::Close(close));
    }
    tasks.extend(kids.iter().rev().map(|&index| Task::Open { index, depth }));
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Style {
    Braces,
    Python,
    Ruby,
    Outline,
    Sql,
}

struct Renderer<'a> {
    parsed: &'a ParsedFile,
    children: &'a [Vec<usize>],
}

impl Renderer<'_> {
    fn style(&self) -> Style {
        match self.parsed.language {
            Language::Python => Style::Python,
            Language::Ruby => Style::Ruby,
            Language::Sql => Style::Sql,
            Language::Protobuf => Style::Braces,
            _ if self.parsed.tier == Tier::Contract => Style::Outline,
            _ => Style::Braces,
        }
    }

    fn doc_prefix(&self) -> &'static str {
        match self.parsed.language {
            Language::Rust | Language::CSharp | Language::Swift | Language::Dart => "///",
            Language::Ruby | Language::Bash | Language::Python => "#",
            Language::Sql => "--",
            _ => "//",
        }
    }

    /// Renders one symbol's own lines and queues its members.
    fn open(&self, index: usize, depth: usize, out: &mut String, tasks: &mut Vec<Task>) {
        let Some(symbol) = self.parsed.symbols.get(index) else {
            return;
        };
        let kids = self.children.get(index).map_or(&[][..], Vec::as_slice);
        let pad = indent(depth);
        match self.style() {
            Style::Outline => {
                self.outline(symbol, depth, out);
                queue(tasks, kids, depth + 1, None);
            }
            Style::Sql => self.sql(symbol, kids, &pad, out),
            Style::Python => {
                push_lines(out, &pad, &symbol.signature, ":");
                let inner = indent(depth + 1);
                if let Some(doc) = summary(symbol.doc.as_deref(), DOC_LINES) {
                    out.push_str(&format!("{inner}\"\"\"{}\"\"\"\n", doc.replace('\n', " ")));
                }
                if !kids.is_empty() {
                    queue(tasks, kids, depth + 1, None);
                } else if symbol.has_body && symbol.doc.is_none() {
                    out.push_str(&format!("{inner}...\n"));
                } else if !symbol.has_body {
                    // Fields and constants: drop the ':' added above.
                    trim_suffix(out, ":\n");
                    out.push('\n');
                }
            }
            Style::Ruby => {
                self.doc(symbol, &pad, out);
                if kids.is_empty() {
                    let suffix = if symbol.has_body { " … end" } else { "" };
                    push_lines(out, &pad, &symbol.signature, suffix);
                } else {
                    push_lines(out, &pad, &symbol.signature, "");
                    queue(tasks, kids, depth + 1, Some(format!("{pad}end\n")));
                }
            }
            Style::Braces => {
                self.doc(symbol, &pad, out);
                if kids.is_empty() {
                    let suffix = if symbol.has_body { " { … }" } else { "" };
                    push_lines(out, &pad, &symbol.signature, suffix);
                } else if symbol.signature.trim_end().ends_with(';') {
                    // File-scoped namespace: members follow at the same level.
                    push_lines(out, &pad, &symbol.signature, "");
                    queue(tasks, kids, depth, None);
                } else {
                    push_lines(out, &pad, &symbol.signature, " {");
                    queue(tasks, kids, depth + 1, Some(format!("{pad}}}\n")));
                }
            }
        }
    }

    fn doc(&self, symbol: &Symbol, pad: &str, out: &mut String) {
        if let Some(doc) = summary(symbol.doc.as_deref(), DOC_LINES) {
            let prefix = self.doc_prefix();
            for line in doc.lines() {
                out.push_str(&format!("{pad}{prefix} {line}\n"));
            }
        }
    }

    fn outline(&self, symbol: &Symbol, depth: usize, out: &mut String) {
        let pad = if self.parsed.language == Language::Markdown {
            String::new()
        } else {
            "  ".repeat(depth.min(MAX_INDENT_DEPTH))
        };
        let doc = summary(symbol.doc.as_deref(), 1)
            .map(|d| format!(" — {d}"))
            .unwrap_or_default();
        let signature = one_line(&symbol.signature);
        out.push_str(&format!("{pad}{signature}{doc}\n"));
    }

    fn sql(&self, symbol: &Symbol, kids: &[usize], pad: &str, out: &mut String) {
        self.doc(symbol, pad, out);
        if symbol.kind == SymbolKind::Column && symbol.parent.is_none() {
            // Added by ALTER TABLE: the qualified name carries the table.
            let table = symbol
                .qualified_name
                .rsplit_once('.')
                .map_or("?", |(table, _)| table);
            out.push_str(&format!(
                "{pad}ALTER TABLE {table} ADD COLUMN {};\n",
                one_line(&symbol.signature)
            ));
            return;
        }
        if kids.is_empty() {
            push_lines(out, pad, &symbol.signature, ";");
            return;
        }
        push_lines(out, pad, &symbol.signature, " (");
        let count = kids.len();
        for (i, &kid) in kids.iter().enumerate() {
            if let Some(column) = self.parsed.symbols.get(kid) {
                let comma = if i + 1 < count { "," } else { "" };
                out.push_str(&format!(
                    "{pad}{INDENT}{}{comma}\n",
                    one_line(&column.signature)
                ));
            }
        }
        out.push_str(&format!("{pad});\n"));
    }
}

/// Appends a (possibly multi-line) signature, indenting every line, with
/// `suffix` after the last line.
fn push_lines(out: &mut String, pad: &str, signature: &str, suffix: &str) {
    let mut lines = signature.lines().peekable();
    while let Some(line) = lines.next() {
        out.push_str(pad);
        out.push_str(line);
        if lines.peek().is_none() {
            out.push_str(suffix);
        }
        out.push('\n');
    }
    if signature.is_empty() {
        out.push_str(pad);
        out.push_str(suffix);
        out.push('\n');
    }
}

fn trim_suffix(out: &mut String, suffix: &str) {
    if out.ends_with(suffix) {
        out.truncate(out.len() - suffix.len());
    }
}
