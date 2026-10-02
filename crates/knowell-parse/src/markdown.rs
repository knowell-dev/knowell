//! Markdown headings as section symbols.
//!
//! The section of a heading runs to the next heading of the same or a
//! higher level, so sections nest by level regardless of how the grammar
//! groups setext headings.

use streaming_iterator::StreamingIterator;
use tree_sitter::{Node, Query, QueryCursor, Tree};

use crate::extract::{Budget, Draft};
use crate::model::{Degradation, SymbolKind};
use crate::text::{bounded, one_line, slice};

const MAX_TITLE_BYTES: usize = 200;

struct Heading {
    level: usize,
    title: String,
    start: usize,
    title_start: usize,
}

pub(crate) fn headings(
    tree: &Tree,
    text: &str,
    query: &Query,
    budget: Budget<'_>,
    max_symbols: usize,
) -> (Vec<Draft>, Option<Degradation>) {
    let mut found: Vec<Heading> = Vec::new();
    let mut degraded = None;
    let mut cursor = QueryCursor::new();
    {
        let mut matches = cursor.matches(query, tree.root_node(), text.as_bytes());
        while let Some(m) = matches.next() {
            for capture in m.captures() {
                if let Some(heading) = heading(capture.node, text) {
                    found.push(heading);
                }
            }
            if found.len() > max_symbols {
                degraded = Some(Degradation::Truncated { limit: max_symbols });
                break;
            }
            if let Some(reason) = budget.exceeded() {
                degraded = Some(reason);
                break;
            }
        }
    }
    found.sort_by_key(|h| h.start);
    found.dedup_by_key(|h| h.start);

    // Close every open section deeper than or level with the next heading.
    let mut ends = vec![text.len(); found.len()];
    let mut open: Vec<usize> = Vec::new();
    for (index, heading) in found.iter().enumerate() {
        while let Some(&top) = open.last() {
            let top_level = found.get(top).map_or(0, |h| h.level);
            if top_level < heading.level {
                break;
            }
            if let Some(end) = ends.get_mut(top) {
                *end = heading.start;
            }
            open.pop();
        }
        open.push(index);
    }
    let drafts = found
        .into_iter()
        .zip(ends)
        .map(|(heading, end)| {
            let signature = format!("{} {}", "#".repeat(heading.level), heading.title);
            let mut draft = Draft::simple(
                SymbolKind::Heading,
                heading.title,
                heading.start..end,
                signature,
            );
            draft.name_start = heading.title_start;
            draft
        })
        .collect();
    (drafts, degraded)
}

fn heading(node: Node<'_>, text: &str) -> Option<Heading> {
    let mut level = 0;
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        let kind = child.kind();
        level = match kind {
            "setext_h1_underline" => 1,
            "setext_h2_underline" => 2,
            _ => kind
                .strip_prefix("atx_h")
                .and_then(|rest| rest.strip_suffix("_marker"))
                .and_then(|digit| digit.parse().ok())
                .unwrap_or(level),
        };
    }
    let content = node.child_by_field_name("heading_content")?;
    let title = one_line(slice(text, &content.byte_range()));
    let title = title.trim_end_matches('#').trim();
    if level == 0 || title.is_empty() {
        return None;
    }
    Some(Heading {
        level,
        title: bounded(title, MAX_TITLE_BYTES),
        start: node.start_byte(),
        title_start: content.start_byte(),
    })
}
