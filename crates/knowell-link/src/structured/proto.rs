//! Protocol Buffers services: one RPC definition per `rpc`, keyed
//! `package.Service/Method`, with a hash of the request and response
//! message structure (field numbers, names, types; nested messages of the
//! same file expanded).

use std::collections::BTreeMap;

use knowell_graph::{ContractKind, EvidenceType};
use knowell_parse::Language;
use knowell_parse::tree_sitter::Node;

use super::{Ctx, hash_hex, tree};
use crate::model::{Extraction, Role};

const SCHEMA_HASH: &str = knowell_graph::ATTR_SCHEMA_HASH;
/// Attribute: request message type of an RPC.
const ATTR_REQUEST: &str = "request";
/// Attribute: response message type of an RPC.
const ATTR_RESPONSE: &str = "response";

fn text_of<'a>(node: Node<'_>, text: &'a str) -> &'a str {
    text.get(node.byte_range()).unwrap_or("")
}

fn named_children(node: Node<'_>) -> Vec<Node<'_>> {
    let mut cursor = node.walk();
    node.named_children(&mut cursor).collect()
}

fn child_of_kind<'t>(node: Node<'t>, kind: &str) -> Option<Node<'t>> {
    named_children(node).into_iter().find(|c| c.kind() == kind)
}

/// `name -> [(number, name, type, repeated)]` for every message in the file.
fn messages(root: Node<'_>, text: &str) -> BTreeMap<String, Vec<(u64, String, String, bool)>> {
    let mut out = BTreeMap::new();
    let mut stack = vec![(root, 0usize)];
    while let Some((node, depth)) = stack.pop() {
        if depth > tree::MAX_DEPTH {
            continue;
        }
        if node.kind() == "message"
            && let Some(name) = child_of_kind(node, "message_name")
        {
            let mut fields = Vec::new();
            if let Some(body) = child_of_kind(node, "message_body") {
                for field in named_children(body) {
                    let fields_here: Vec<Node<'_>> = if field.kind() == "oneof" {
                        named_children(field)
                            .into_iter()
                            .filter(|f| f.kind() == "oneof_field")
                            .collect()
                    } else if matches!(field.kind(), "field" | "map_field") {
                        vec![field]
                    } else {
                        Vec::new()
                    };
                    for f in fields_here {
                        let field_name = named_children(f)
                            .into_iter()
                            .find(|c| c.kind() == "identifier")
                            .map(|c| text_of(c, text).to_owned())
                            .unwrap_or_default();
                        let field_type = named_children(f)
                            .into_iter()
                            .find(|c| matches!(c.kind(), "type" | "key_type"))
                            .map(|c| text_of(c, text).trim().to_owned())
                            .unwrap_or_else(|| "map".to_owned());
                        let number = child_of_kind(f, "field_number")
                            .and_then(|n| text_of(n, text).trim().parse::<u64>().ok())
                            .unwrap_or(0);
                        let repeated = text_of(f, text).trim_start().starts_with("repeated");
                        fields.push((number, field_name, field_type, repeated));
                    }
                }
            }
            fields.sort();
            out.insert(text_of(name, text).trim().to_owned(), fields);
        }
        for child in named_children(node) {
            stack.push((child, depth + 1));
        }
    }
    out
}

fn describe(
    name: &str,
    all: &BTreeMap<String, Vec<(u64, String, String, bool)>>,
    depth: usize,
) -> String {
    let short = name.rsplit('.').next().unwrap_or(name);
    let Some(fields) = all.get(short) else {
        return name.to_owned();
    };
    if depth > 3 {
        return format!("{short}{{..}}");
    }
    let body: Vec<String> = fields
        .iter()
        .map(|(number, field, ty, repeated)| {
            format!(
                "{number}:{field}:{}{}",
                if *repeated { "repeated " } else { "" },
                describe(ty, all, depth + 1)
            )
        })
        .collect();
    format!("{short}{{{}}}", body.join(";"))
}

/// RPC definitions of a `.proto` file.
pub(crate) fn extract(ctx: &Ctx<'_>, text: &str) -> Vec<Extraction> {
    if ctx.path.extension() != Some("proto") {
        return Vec::new();
    }
    let Some(tree) = ctx.tree(Language::Protobuf, text) else {
        return Vec::new();
    };
    let root = tree.root_node();
    let package = named_children(root)
        .into_iter()
        .find(|c| c.kind() == "package")
        .and_then(|p| child_of_kind(p, "full_ident"))
        .map(|f| text_of(f, text).trim().to_owned())
        .unwrap_or_default();
    let all = messages(root, text);
    let mut out = Vec::new();
    for service in named_children(root)
        .into_iter()
        .filter(|c| c.kind() == "service")
    {
        let Some(service_name) = child_of_kind(service, "service_name") else {
            continue;
        };
        let service_name = text_of(service_name, text).trim();
        let qualified = if package.is_empty() {
            service_name.to_owned()
        } else {
            format!("{package}.{service_name}")
        };
        for rpc in named_children(service)
            .into_iter()
            .filter(|c| c.kind() == "rpc")
        {
            let Some(rpc_name) = child_of_kind(rpc, "rpc_name") else {
                continue;
            };
            let types: Vec<String> = named_children(rpc)
                .into_iter()
                .filter(|c| c.kind() == "message_or_enum_type")
                .map(|c| text_of(c, text).trim().to_owned())
                .collect();
            let request = types.first().cloned().unwrap_or_default();
            let response = types.get(1).cloned().unwrap_or_default();
            let signature = text_of(rpc, text);
            let streams = signature.matches("stream ").count().to_string();
            let hash = hash_hex(
                "proto-rpc/v1",
                &[
                    &describe(&request, &all, 0),
                    &describe(&response, &all, 0),
                    &streams,
                ],
            );
            let mut attrs = BTreeMap::new();
            attrs.insert(SCHEMA_HASH.to_owned(), hash);
            attrs.insert(ATTR_REQUEST.to_owned(), request);
            attrs.insert(ATTR_RESPONSE.to_owned(), response);
            let Some(range) = tree::line(rpc) else {
                continue;
            };
            let key = format!("{qualified}/{}", text_of(rpc_name, text).trim());
            out.extend(ctx.extraction(
                ContractKind::Rpc,
                Role::Definition,
                &key,
                range,
                None,
                EvidenceType::ContractDerived,
                attrs,
            ));
        }
    }
    out
}
