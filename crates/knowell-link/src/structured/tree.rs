//! Read-only views over YAML and JSON syntax trees.
//!
//! Callers walk maps and sequences and decode only the scalars they ask for;
//! nothing is materialised eagerly, so an extractor that reads keys never
//! decodes the values next to them (environment values in Compose and
//! Kubernetes files stay untouched).

use knowell_core::LineRange;
use knowell_parse::tree_sitter::Node;

use crate::extract::decode_unquote;

/// Deepest nesting followed by recursive helpers.
pub(crate) const MAX_DEPTH: usize = 64;

/// The value nodes of a document: one per YAML document, one for JSON.
pub(crate) fn roots(root: Node<'_>) -> Vec<Node<'_>> {
    let mut cursor = root.walk();
    let children: Vec<Node<'_>> = root.named_children(&mut cursor).collect();
    let mut out = Vec::new();
    match root.kind() {
        "stream" => {
            for child in children {
                if child.kind() == "document"
                    && let Some(value) = first_value(child)
                {
                    out.push(value);
                }
            }
        }
        "document" => {
            if let Some(value) = children.into_iter().find(|c| c.kind() != "comment") {
                out.push(unwrap(value));
            }
        }
        _ => out.push(unwrap(root)),
    }
    out
}

fn first_value(node: Node<'_>) -> Option<Node<'_>> {
    let mut cursor = node.walk();
    let found = node
        .named_children(&mut cursor)
        .find(|c| c.kind() != "comment");
    found.map(unwrap)
}

/// Skips YAML wrapper nodes (`block_node`, `flow_node`, anchors, tags).
pub(crate) fn unwrap(node: Node<'_>) -> Node<'_> {
    let mut current = node;
    for _ in 0..16 {
        match current.kind() {
            "block_node" | "flow_node" => {
                let mut cursor = current.walk();
                let inner = current
                    .named_children(&mut cursor)
                    .find(|c| !matches!(c.kind(), "anchor" | "tag" | "comment"));
                match inner {
                    Some(inner) => current = inner,
                    None => return current,
                }
            }
            _ => return current,
        }
    }
    current
}

/// One map entry.
#[derive(Clone, Copy)]
pub(crate) struct Entry<'t> {
    pub(crate) key: Node<'t>,
    pub(crate) value: Option<Node<'t>>,
}

impl<'t> Entry<'t> {
    pub(crate) fn key_text(&self, text: &str) -> String {
        scalar(self.key, text).unwrap_or_default()
    }
}

/// Entries of a YAML block / flow mapping or a JSON object.
pub(crate) fn entries(node: Node<'_>) -> Vec<Entry<'_>> {
    let node = unwrap(node);
    let pair_kind = match node.kind() {
        "block_mapping" => "block_mapping_pair",
        "flow_mapping" => "flow_pair",
        "object" => "pair",
        _ => return Vec::new(),
    };
    let mut cursor = node.walk();
    let mut out = Vec::new();
    for child in node.named_children(&mut cursor) {
        if child.kind() != pair_kind {
            continue;
        }
        let Some(key) = child.child_by_field_name("key") else {
            continue;
        };
        out.push(Entry {
            key: unwrap(key),
            value: child.child_by_field_name("value").map(unwrap),
        });
    }
    out
}

/// The value under `key` in a mapping.
pub(crate) fn get<'t>(node: Node<'t>, key: &str, text: &str) -> Option<Node<'t>> {
    entries(node)
        .into_iter()
        .find(|e| e.key_text(text) == key)
        .and_then(|e| e.value)
}

/// Items of a YAML sequence or a JSON array.
pub(crate) fn items(node: Node<'_>) -> Vec<Node<'_>> {
    let node = unwrap(node);
    let mut cursor = node.walk();
    match node.kind() {
        "block_sequence" => node
            .named_children(&mut cursor)
            .filter(|c| c.kind() == "block_sequence_item")
            .filter_map(|item| {
                let mut inner = item.walk();
                let found = item
                    .named_children(&mut inner)
                    .find(|c| c.kind() != "comment");
                found.map(unwrap)
            })
            .collect(),
        "flow_sequence" | "array" => node
            .named_children(&mut cursor)
            .filter(|c| c.kind() != "comment")
            .map(unwrap)
            .collect(),
        _ => Vec::new(),
    }
}

/// Whether the node is a mapping / object.
pub(crate) fn is_map(node: Node<'_>) -> bool {
    matches!(
        unwrap(node).kind(),
        "block_mapping" | "flow_mapping" | "object"
    )
}

/// Decodes a scalar (plain, quoted, block; JSON string, number, literal).
pub(crate) fn scalar(node: Node<'_>, text: &str) -> Option<String> {
    let node = unwrap(node);
    let raw = text.get(node.byte_range())?;
    match node.kind() {
        "plain_scalar" | "string_scalar" | "integer_scalar" | "float_scalar" | "boolean_scalar"
        | "null_scalar" | "number" | "true" | "false" | "null" => Some(raw.trim().to_owned()),
        "double_quote_scalar" | "single_quote_scalar" | "string" => Some(decode_unquote(raw)),
        "block_scalar" => Some(
            raw.lines()
                .skip(1)
                .map(str::trim)
                .collect::<Vec<_>>()
                .join("\n"),
        ),
        _ => None,
    }
}

/// The first line of a node as a range.
pub(crate) fn line(node: Node<'_>) -> Option<LineRange> {
    let start = u32::try_from(node.start_position().row)
        .ok()?
        .checked_add(1)?;
    LineRange::new(start, start).ok()
}

/// Canonical JSON-like text of a subtree for hashing: maps sorted by key,
/// documentation keys (`description`, `summary`, `example`, `examples`,
/// `title`) dropped, `$ref` replaced by the referenced subtree (resolved in
/// `root`, bounded by depth so cycles terminate).
pub(crate) fn canonical(node: Node<'_>, root: Node<'_>, text: &str, depth: usize) -> String {
    if depth > 24 {
        return "\"<depth>\"".to_owned();
    }
    let node = unwrap(node);
    if is_map(node) {
        let mut pairs: Vec<(String, String)> = Vec::new();
        for entry in entries(node) {
            let key = entry.key_text(text);
            if matches!(
                key.as_str(),
                "description" | "summary" | "example" | "examples" | "title" | "x-examples"
            ) {
                continue;
            }
            if key == "$ref"
                && let Some(target) = entry
                    .value
                    .and_then(|v| scalar(v, text))
                    .and_then(|r| resolve_ref(root, &r, text))
            {
                return canonical(target, root, text, depth + 1);
            }
            let value = entry.value.map_or_else(
                || "null".to_owned(),
                |v| canonical(v, root, text, depth + 1),
            );
            pairs.push((key, value));
        }
        pairs.sort();
        let body: Vec<String> = pairs
            .into_iter()
            .map(|(k, v)| format!("{}:{v}", serde_json::Value::String(k)))
            .collect();
        return format!("{{{}}}", body.join(","));
    }
    let list = items(node);
    if !list.is_empty() || matches!(node.kind(), "block_sequence" | "flow_sequence" | "array") {
        let body: Vec<String> = list
            .into_iter()
            .map(|item| canonical(item, root, text, depth + 1))
            .collect();
        return format!("[{}]", body.join(","));
    }
    serde_json::Value::String(scalar(node, text).unwrap_or_default()).to_string()
}

/// Resolves a local `$ref` (`#/components/schemas/X`).
pub(crate) fn resolve_ref<'t>(root: Node<'t>, reference: &str, text: &str) -> Option<Node<'t>> {
    let path = reference.strip_prefix("#/")?;
    let mut current = root;
    for part in path.split('/') {
        let part = part.replace("~1", "/").replace("~0", "~");
        current = get(current, &part, text)?;
    }
    Some(current)
}
