//! Source-only Rust uses. Lexical bindings suppress unrelated name matches;
//! receiver types, macros and compiler configuration are not inferred.

use std::collections::{BTreeMap, BTreeSet};

use knowell_parse::tree_sitter::Node;

use crate::references::{
    Identifier, IdentifierScan, MAX_IDENTIFIERS, MAX_NODES, ReferenceCoverage, UseKind,
};

const MAX_ANCESTORS: usize = 256;

#[derive(Clone, Copy)]
struct Binding {
    start: usize,
    end: usize,
}

type Bindings = BTreeMap<(usize, String, bool), Vec<Binding>>;

fn scope(node: Node<'_>) -> bool {
    matches!(
        node.kind(),
        "block"
            | "function_item"
            | "function_signature_item"
            | "closure_expression"
            | "match_arm"
            | "for_expression"
            | "if_expression"
            | "while_expression"
            | "mod_item"
            | "impl_item"
            | "trait_item"
            | "struct_item"
            | "enum_item"
            | "source_file"
    )
}

fn nearest_scope(mut node: Node<'_>) -> Option<Node<'_>> {
    for _ in 0..MAX_ANCESTORS {
        if scope(node) {
            return Some(node);
        }
        node = node.parent()?;
    }
    None
}

fn text_of<'a>(node: Node<'_>, text: &'a str) -> Option<&'a str> {
    text.get(node.byte_range())
}

fn qualified_text(node: Node<'_>, text: &str) -> String {
    // Complex receivers remain unresolvable, without retaining an entire body
    // once for every member access in hostile input.
    text_of(node, text)
        .filter(|s| s.len() <= 1_024)
        .map(|s| s.replace("::", "."))
        .unwrap_or_default()
}

#[allow(clippy::too_many_arguments)]
fn add_pattern(
    pattern: Node<'_>,
    owner: Node<'_>,
    start: usize,
    end: usize,
    type_name: bool,
    text: &str,
    bindings: &mut Bindings,
    declarations: &mut BTreeSet<(usize, usize)>,
    coverage: &mut ReferenceCoverage,
) {
    let mut cursor = pattern.walk();
    let mut visited = 0usize;
    'walk: loop {
        visited = visited.saturating_add(1);
        if visited > MAX_NODES || declarations.len() >= MAX_IDENTIFIERS {
            coverage.truncated = true;
            break;
        }
        let node = cursor.node();
        let name_node = node.kind() == "identifier"
            || node.kind() == "shorthand_field_identifier"
            || (type_name && node.kind() == "type_identifier");
        // Paths in enum/struct patterns name constructors, not new bindings.
        let path_name = node.parent().is_some_and(|p| {
            matches!(
                p.kind(),
                "scoped_identifier" | "scoped_type_identifier" | "generic_type" | "generic_pattern"
            )
        });
        if name_node
            && !path_name
            && let Some(name) = text_of(node, text)
        {
            let name = name.strip_prefix("r#").unwrap_or(name);
            declarations.insert((node.start_byte(), node.end_byte()));
            bindings
                .entry((owner.id(), name.to_owned(), type_name))
                .or_default()
                .push(Binding { start, end });
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
}

fn collect_binding(
    node: Node<'_>,
    text: &str,
    bindings: &mut Bindings,
    declarations: &mut BTreeSet<(usize, usize)>,
    coverage: &mut ReferenceCoverage,
) {
    let parent = node.parent();
    let plan = match node.kind() {
        "let_declaration" => parent.and_then(nearest_scope).and_then(|owner| {
            node.child_by_field_name("pattern")
                .map(|p| (p, owner, node.end_byte(), owner.end_byte(), false))
        }),
        "parameter" => parent.and_then(nearest_scope).and_then(|owner| {
            node.child_by_field_name("pattern")
                .map(|p| (p, owner, node.end_byte(), owner.end_byte(), false))
        }),
        "closure_parameters" => {
            parent.map(|owner| (node, owner, node.end_byte(), owner.end_byte(), false))
        }
        "for_expression" => node
            .child_by_field_name("pattern")
            .zip(node.child_by_field_name("body"))
            .map(|(p, body)| (p, node, body.start_byte(), body.end_byte(), false)),
        "match_arm" => node
            .child_by_field_name("pattern")
            .map(|p| (p, node, p.end_byte(), node.end_byte(), false)),
        "let_condition" => parent.and_then(nearest_scope).and_then(|owner| {
            let body = owner
                .child_by_field_name("consequence")
                .or_else(|| owner.child_by_field_name("body"));
            node.child_by_field_name("pattern")
                .zip(body)
                .map(|(p, body)| (p, owner, node.end_byte(), body.end_byte(), false))
        }),
        "type_parameter" => parent.and_then(nearest_scope).and_then(|owner| {
            node.child_by_field_name("name")
                .map(|p| (p, owner, owner.start_byte(), owner.end_byte(), true))
        }),
        _ => None,
    };
    if let Some((pattern, owner, start, end, type_name)) = plan {
        add_pattern(
            pattern,
            owner,
            start,
            end,
            type_name,
            text,
            bindings,
            declarations,
            coverage,
        );
    }
}

fn declaration(node: Node<'_>) -> bool {
    let Some(parent) = node.parent() else {
        return false;
    };
    matches!(
        parent.kind(),
        "function_item"
            | "function_signature_item"
            | "struct_item"
            | "union_item"
            | "enum_item"
            | "trait_item"
            | "mod_item"
            | "type_item"
            | "associated_type"
            | "const_item"
            | "static_item"
            | "macro_definition"
            | "field_declaration"
            | "enum_variant"
    ) && parent
        .child_by_field_name("name")
        .is_some_and(|name| name.id() == node.id())
}

fn excluded(mut node: Node<'_>) -> bool {
    for _ in 0..MAX_ANCESTORS {
        if matches!(
            node.kind(),
            "use_declaration"
                | "extern_crate_declaration"
                | "attribute_item"
                | "inner_attribute_item"
                | "token_tree"
                | "macro_rule"
        ) {
            return true;
        }
        let Some(parent) = node.parent() else {
            return false;
        };
        node = parent;
    }
    true
}

fn role(node: Node<'_>) -> UseKind {
    if declaration(node) {
        return UseKind::Declaration;
    }
    let mut part = node;
    for _ in 0..MAX_ANCESTORS {
        let Some(parent) = part.parent() else { break };
        if matches!(
            parent.kind(),
            "scoped_identifier" | "scoped_type_identifier"
        ) && !parent
            .child_by_field_name("name")
            .is_some_and(|n| n.id() == part.id())
        {
            return UseKind::Qualifier;
        }
        if parent.kind() == "macro_invocation" {
            return UseKind::Macro;
        }
        if !matches!(
            parent.kind(),
            "scoped_identifier" | "scoped_type_identifier"
        ) {
            break;
        }
        part = parent;
    }
    if node.kind() == "type_identifier" {
        UseKind::Type
    } else {
        UseKind::Value
    }
}

fn callee_and_path(node: Node<'_>, text: &str) -> (bool, bool, Option<String>) {
    let mut top = node;
    let mut member = false;
    let mut qualified = None;
    if let Some(parent) = node.parent() {
        if matches!(
            parent.kind(),
            "scoped_identifier" | "scoped_type_identifier"
        ) && parent
            .child_by_field_name("name")
            .is_some_and(|n| n.id() == node.id())
        {
            top = parent;
            qualified = Some(qualified_text(parent, text));
        } else if parent.kind() == "field_expression"
            && parent
                .child_by_field_name("field")
                .is_some_and(|n| n.id() == node.id())
        {
            top = parent;
            member = true;
            let receiver = parent
                .child_by_field_name("value")
                .map(|n| qualified_text(n, text));
            qualified = receiver
                .zip(text_of(node, text))
                .map(|(r, n)| format!("{r}.{n}"));
        }
    }
    for _ in 0..MAX_ANCESTORS {
        let Some(parent) = top.parent() else {
            return (false, member, qualified);
        };
        if parent.kind() == "generic_function"
            && parent
                .child_by_field_name("function")
                .is_some_and(|n| n.id() == top.id())
        {
            top = parent;
            continue;
        }
        return (
            parent.kind() == "call_expression"
                && parent
                    .child_by_field_name("function")
                    .is_some_and(|n| n.id() == top.id()),
            member,
            qualified,
        );
    }
    (false, member, qualified)
}

fn scopes(mut node: Node<'_>) -> (Vec<usize>, Vec<usize>) {
    let mut values = Vec::new();
    let mut types = Vec::new();
    let mut crossed_function = false;
    for _ in 0..MAX_ANCESTORS {
        let function = matches!(node.kind(), "function_item" | "function_signature_item");
        if function && crossed_function {
            break;
        }
        if scope(node) {
            if !crossed_function {
                values.push(node.id());
            }
            types.push(node.id());
        }
        if function {
            crossed_function = true;
        }
        let Some(parent) = node.parent() else { break };
        node = parent;
    }
    (values, types)
}

/// Retains exact call spans even when lexical syntax cannot resolve their targets.
pub(crate) fn scan(root: Node<'_>, text: &str) -> IdentifierScan {
    let mut coverage = ReferenceCoverage::default();
    let mut bindings = Bindings::new();
    let mut declarations = BTreeSet::new();
    let mut pending = Vec::new();
    let mut cursor = root.walk();
    let mut visited = 0usize;
    'walk: loop {
        visited = visited.saturating_add(1);
        if visited > MAX_NODES || pending.len() >= MAX_IDENTIFIERS {
            coverage.truncated = true;
            break;
        }
        let node = cursor.node();
        collect_binding(node, text, &mut bindings, &mut declarations, &mut coverage);
        if node.child_count() == 0
            && node.is_named()
            && node.kind().ends_with("identifier")
            && !excluded(node)
            && let Some(name) = text_of(node, text)
        {
            let name = name.strip_prefix("r#").unwrap_or(name);
            if !super::references::plausible(name) && name.chars().count() != 1 {
                if name.len() > 128 {
                    coverage.truncated = true;
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
            let (is_call, member, qualified) = callee_and_path(node, text);
            let kind = role(node);
            let owner = if kind == UseKind::Declaration {
                node.parent()
                    .and_then(|p| p.parent())
                    .and_then(nearest_scope)
            } else {
                nearest_scope(node)
            };
            pending.push((
                Identifier {
                    name: name.to_owned(),
                    line: u32::try_from(node.start_position().row)
                        .unwrap_or(u32::MAX)
                        .saturating_add(1),
                    member,
                    start_byte: node.start_byte(),
                    end_byte: node.end_byte(),
                    is_call,
                    qualified: qualified.map(|q| {
                        q.split('.')
                            .map(|s| s.strip_prefix("r#").unwrap_or(s))
                            .collect::<Vec<_>>()
                            .join(".")
                    }),
                    kind,
                    shadowed: false,
                    scope_start_byte: owner.map_or(0, |n| n.start_byte()),
                    scope_end_byte: owner.map_or(text.len(), |n| n.end_byte()),
                },
                scopes(node),
            ));
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
    for values in bindings.values_mut() {
        values.sort_by_key(|b| b.start);
    }
    let mut identifiers = Vec::with_capacity(pending.len());
    for (mut ident, (value_scopes, type_scopes)) in pending {
        if declarations.contains(&(ident.start_byte, ident.end_byte)) {
            ident.kind = UseKind::Declaration;
        } else if !ident.member {
            let name = ident
                .qualified
                .as_deref()
                .and_then(|q| q.split('.').next())
                .unwrap_or(&ident.name);
            let type_name = ident.qualified.is_some() || ident.kind == UseKind::Type;
            let scopes = if type_name {
                &type_scopes
            } else {
                &value_scopes
            };
            ident.shadowed = scopes.iter().any(|scope| {
                bindings
                    .get(&(*scope, name.to_owned(), type_name))
                    .is_some_and(|bindings| {
                        let end = bindings.partition_point(|b| b.start <= ident.start_byte);
                        // Bindings in one lexical owner have the same visibility end;
                        // binary lookup also bounds hostile repeated-name input.
                        end.checked_sub(1)
                            .and_then(|last| bindings.get(last))
                            .is_some_and(|b| ident.start_byte < b.end)
                    })
            });
        }
        identifiers.push(ident);
    }
    identifiers.sort();
    coverage.scanned = identifiers.len();
    coverage.call_sites = identifiers.iter().filter(|i| i.is_call).count();
    coverage.shadowed = identifiers.iter().filter(|i| i.shadowed).count();
    IdentifierScan {
        identifiers,
        coverage,
    }
}
