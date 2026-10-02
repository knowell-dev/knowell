//! Turning captured syntax nodes into key text.
//!
//! Decoding is grammar-agnostic: it relies on the node-kind naming
//! conventions shared by the bundled tree-sitter grammars (`*string*`,
//! `*interpolation*` / `*substitution*`, `*identifier*`, `member_expression`,
//! `binary_expression` with `+`, arrays and lists). Anything it does not
//! understand becomes a dynamic part, never a guess.

use knowell_parse::tree_sitter::Node;

use crate::model::DYN;

/// Most alternatives a decoded expression may produce.
const MAX_ALTERNATIVES: usize = 64;
/// Deepest expression nesting followed.
const MAX_DEPTH: usize = 8;

/// A piece of a value whose constants are resolved later.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum Piece {
    /// Literal text.
    Lit(String),
    /// A runtime-dynamic part.
    Dyn,
    /// A reference to a named constant (`SUBSCRIPTION_CANCELLED`,
    /// `Topics.CREATED`), resolved through constant bindings.
    Ref(String),
}

/// Alternatives (lists fan out), each a sequence of pieces.
pub(crate) type Pieces = Vec<Vec<Piece>>;

fn node_text<'a>(node: Node<'_>, text: &'a str) -> &'a str {
    text.get(node.byte_range()).unwrap_or("")
}

pub(crate) fn is_string_kind(kind: &str) -> bool {
    (kind.contains("string") || kind == "template_chars_single_single")
        && !kind.contains("type")
        && kind != "string_start"
        && kind != "string_end"
}

fn is_identifier_kind(kind: &str) -> bool {
    kind == "identifier"
        || kind == "constant"
        || kind.ends_with("_identifier")
        || kind == "identifier_dollar_escaped"
}

fn is_member_kind(kind: &str) -> bool {
    matches!(
        kind,
        "member_expression"
            | "field_expression"
            | "selector_expression"
            | "attribute"
            | "navigation_expression"
            | "field_access"
            | "member_access_expression"
            | "scoped_identifier"
            | "qualified_identifier"
            | "qualified_name"
            | "scoped_type_identifier"
    )
}

fn is_number_kind(kind: &str) -> bool {
    kind.contains("number")
        || kind.contains("integer")
        || matches!(kind, "int_lit" | "decimal_lit" | "float_lit" | "float")
}

fn is_container_kind(kind: &str) -> bool {
    !kind.contains("type")
        && (kind.contains("array")
            || kind.contains("list")
            || kind.contains("tuple")
            || kind.contains("collection")
            || matches!(
                kind,
                "literal_value"
                    | "composite_literal"
                    | "set_or_map_literal"
                    | "initializer_expression"
                    | "element_value_array_initializer"
            ))
        && !matches!(
            kind,
            "argument_list" | "formal_parameter_list" | "parameter_list"
        )
}

fn is_wrapper_kind(kind: &str) -> bool {
    matches!(
        kind,
        "parenthesized_expression"
            | "await_expression"
            | "unary_expression"
            | "reference_expression"
            | "literal_element"
            | "element"
            | "argument"
            | "value_argument"
            | "postfix_expression"
            | "non_null_expression"
            | "as_expression"
            | "satisfies_expression"
            | "expression_statement"
            | "flow_node"
            | "block_node"
            | "element_value_pair"
    )
}

fn skip_child(kind: &str) -> bool {
    kind.contains("type") || kind == "comment" || kind.contains("label") || kind == "bang"
}

/// Whether a binary node is a `+` concatenation.
fn is_concat(node: Node<'_>) -> bool {
    let kind = node.kind();
    if kind == "concatenated_string" {
        return true;
    }
    if !matches!(
        kind,
        "binary_expression" | "binary_operator" | "additive_expression"
    ) {
        return false;
    }
    let mut cursor = node.walk();
    node.children(&mut cursor)
        .any(|c| !c.is_named() && c.kind() == "+")
}

fn product(left: Pieces, right: Pieces) -> Pieces {
    let mut out = Vec::new();
    for l in &left {
        for r in &right {
            if out.len() >= MAX_ALTERNATIVES {
                return out;
            }
            let mut joined = l.clone();
            joined.extend(r.iter().cloned());
            out.push(joined);
        }
    }
    out
}

/// Decodes a value expression; identifiers become [`Piece::Ref`].
pub(crate) fn decode_expr(node: Node<'_>, text: &str) -> Pieces {
    decode_expr_at(node, text, 0)
}

fn decode_expr_at(node: Node<'_>, text: &str, depth: usize) -> Pieces {
    if depth > MAX_DEPTH {
        return vec![vec![Piece::Dyn]];
    }
    let kind = node.kind();
    if is_string_kind(kind) {
        return vec![string_pieces(node, text, true, depth)];
    }
    if is_identifier_kind(kind) || is_member_kind(kind) {
        let name: String = node_text(node, text)
            .chars()
            .filter(|c| !c.is_whitespace())
            .collect();
        return vec![vec![Piece::Ref(name)]];
    }
    if is_number_kind(kind) {
        return vec![vec![Piece::Lit(node_text(node, text).to_owned())]];
    }
    if is_concat(node) {
        let mut cursor = node.walk();
        let operands: Vec<Node<'_>> = node.named_children(&mut cursor).collect();
        let mut acc: Pieces = vec![Vec::new()];
        for operand in operands {
            acc = product(acc, decode_expr_at(operand, text, depth + 1));
        }
        return acc;
    }
    if is_container_kind(kind) {
        let mut cursor = node.walk();
        let mut out = Vec::new();
        for child in node.named_children(&mut cursor) {
            if skip_child(child.kind()) {
                continue;
            }
            for alternative in decode_expr_at(child, text, depth + 1) {
                if out.len() >= MAX_ALTERNATIVES {
                    break;
                }
                out.push(alternative);
            }
        }
        if out.is_empty() {
            out.push(vec![Piece::Dyn]);
        }
        return out;
    }
    if is_wrapper_kind(kind) {
        let mut cursor = node.walk();
        let inner = node
            .named_children(&mut cursor)
            .filter(|c| !skip_child(c.kind()))
            .last();
        return match inner {
            Some(child) => decode_expr_at(child, text, depth + 1),
            None => vec![vec![Piece::Dyn]],
        };
    }
    vec![vec![Piece::Dyn]]
}

/// Plain text of a capture: strings are decoded (interpolations become
/// dynamic), any other node is its source text.
pub(crate) fn decode_plain(node: Node<'_>, text: &str) -> String {
    if is_string_kind(node.kind()) {
        let pieces = string_pieces(node, text, false, 0);
        return pieces
            .into_iter()
            .map(|p| match p {
                Piece::Lit(s) => s,
                Piece::Dyn | Piece::Ref(_) => DYN.to_string(),
            })
            .collect();
    }
    node_text(node, text).trim().to_owned()
}

/// The pieces of a string literal. With `refs`, a substitution of a lone
/// identifier becomes a [`Piece::Ref`]; otherwise substitutions are dynamic.
fn string_pieces(node: Node<'_>, text: &str, refs: bool, depth: usize) -> Vec<Piece> {
    if node.named_child_count() == 0 {
        return vec![Piece::Lit(unquote(node_text(node, text)))];
    }
    let mut out = Vec::new();
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        if !child.is_named() {
            continue;
        }
        let kind = child.kind();
        if kind == "string_start" || kind == "string_end" {
            continue;
        }
        if kind.contains("interpolat") || kind.contains("substitution") {
            out.push(substitution(child, text, refs));
        } else if kind.contains("escape") {
            out.push(Piece::Lit(unescape(node_text(child, text))));
        } else if child.named_child_count() > 0 && depth < MAX_DEPTH {
            out.extend(string_pieces(child, text, refs, depth + 1));
        } else {
            out.push(Piece::Lit(node_text(child, text).to_owned()));
        }
    }
    merge_literals(out)
}

fn substitution(node: Node<'_>, text: &str, refs: bool) -> Piece {
    if !refs {
        return Piece::Dyn;
    }
    let raw = node_text(node, text);
    let inner = raw
        .trim()
        .trim_start_matches("${")
        .trim_start_matches("\\(")
        .trim_start_matches('$')
        .trim_start_matches('{')
        .trim_end_matches('}')
        .trim_end_matches(')')
        .trim();
    let simple = !inner.is_empty()
        && inner
            .chars()
            .all(|c| c.is_alphanumeric() || c == '_' || c == '.')
        && inner.chars().next().is_some_and(|c| !c.is_ascii_digit());
    if simple {
        Piece::Ref(inner.to_owned())
    } else {
        Piece::Dyn
    }
}

fn merge_literals(pieces: Vec<Piece>) -> Vec<Piece> {
    let mut out: Vec<Piece> = Vec::with_capacity(pieces.len());
    for piece in pieces {
        if let Piece::Lit(text) = &piece
            && let Some(Piece::Lit(prev)) = out.last_mut()
        {
            prev.push_str(text);
            continue;
        }
        out.push(piece);
    }
    out
}

/// Strips string prefixes and quotes (`"x"`, `'x'`, `` `x` ``, `"""x"""`,
/// `r"x"`, `r#"x"#`, `@"x"`, `b'x'`) and decodes common escapes.
pub(crate) fn unquote(raw: &str) -> String {
    let mut s = raw.trim();
    // A prefix (`r`, `b`, `f`, `u`, `@`, `$`, combinations) only counts when
    // a quote or `#` follows it; `users` keeps its leading `u`.
    if let Some(start) = s.find(['"', '\'', '`', '#'])
        && start <= 3
        && s.get(..start)
            .is_some_and(|p| p.chars().all(|c| "rRbBuUfF@$".contains(c)))
    {
        s = s.get(start..).unwrap_or(s);
    }
    let hashes = s.len() - s.trim_start_matches('#').len();
    if hashes > 0 && s.ends_with('#') {
        s = s.get(hashes..).unwrap_or(s);
        s = s.get(..s.len().saturating_sub(hashes)).unwrap_or(s);
    }
    for quote in ["\"\"\"", "'''", "\"", "'", "`"] {
        if s.len() >= 2 * quote.len()
            && let Some(inner) = s.strip_prefix(quote).and_then(|x| x.strip_suffix(quote))
        {
            return unescape(inner);
        }
    }
    s.to_owned()
}

fn unescape(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    let mut chars = raw.chars();
    while let Some(c) = chars.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        match chars.next() {
            Some('n') => out.push('\n'),
            Some('t') => out.push('\t'),
            Some(other) => out.push(other),
            None => out.push('\\'),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unquoting() {
        assert_eq!(unquote("\"a\\\"b\""), "a\"b");
        assert_eq!(unquote("'x'"), "x");
        assert_eq!(unquote("`raw`"), "raw");
        assert_eq!(unquote("r#\"x\"#"), "x");
        assert_eq!(unquote("\"\"\"doc\"\"\""), "doc");
        assert_eq!(unquote("@\"verbatim\""), "verbatim");
        assert_eq!(unquote("plain"), "plain");
        assert_eq!(unquote("users"), "users");
        assert_eq!(unquote("f\"x{y}\""), "x{y}");
        assert_eq!(unquote("\""), "\"");
    }

    #[test]
    fn merging() {
        let merged = merge_literals(vec![
            Piece::Lit("a".into()),
            Piece::Lit("b".into()),
            Piece::Dyn,
            Piece::Lit("c".into()),
        ]);
        assert_eq!(
            merged,
            vec![Piece::Lit("ab".into()), Piece::Dyn, Piece::Lit("c".into())]
        );
    }
}
