//! OpenAPI / Swagger, AsyncAPI and event JSON Schema documents.

use std::collections::BTreeMap;

use knowell_graph::{ContractKind, EvidenceType};
use knowell_parse::tree_sitter::Node;

use super::tree::{self, canonical, entries, get, items, scalar};
use super::{Ctx, data_language, hash_hex};
use crate::model::{ATTR_FIELDS, ATTR_OPERATION, ATTR_VERSION, Extraction, Role};

const HTTP_METHODS: &[&str] = &[
    "get", "put", "post", "delete", "options", "head", "patch", "trace",
];

/// Attribute holding a definition's schema hash (same name as the graph's).
const SCHEMA_HASH: &str = knowell_graph::ATTR_SCHEMA_HASH;

fn document_roots<'t>(tree: &'t knowell_parse::tree_sitter::Tree) -> Vec<Node<'t>> {
    tree::roots(tree.root_node())
        .into_iter()
        .filter(|n| tree::is_map(*n))
        .collect()
}

/// Path part of the first server URL (OpenAPI 3) or `basePath` (Swagger 2),
/// without a trailing slash; empty when absent or templated.
fn base_path(root: Node<'_>, text: &str) -> String {
    let raw = get(root, "basePath", text)
        .and_then(|n| scalar(n, text))
        .or_else(|| {
            get(root, "servers", text)
                .and_then(|servers| items(servers).into_iter().next())
                .and_then(|server| get(server, "url", text))
                .and_then(|url| scalar(url, text))
                .map(|url| match url.split_once("://") {
                    Some((_, rest)) => rest
                        .find('/')
                        .map_or(String::new(), |i| rest.get(i..).unwrap_or("").to_owned()),
                    None => url,
                })
        })
        .unwrap_or_default();
    if raw.contains('{') || !raw.starts_with('/') {
        return String::new();
    }
    raw.trim_end_matches('/').to_owned()
}

/// Endpoints of an OpenAPI / Swagger document, one per path x method, with
/// the hash of the operation's parameters, request body and responses
/// (`$ref`s resolved, documentation fields ignored).
pub(crate) fn openapi(ctx: &Ctx<'_>, text: &str) -> Vec<Extraction> {
    let Some(language) = data_language(ctx.path) else {
        return Vec::new();
    };
    let Some(tree) = ctx.tree(language, text) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for root in document_roots(&tree) {
        let is_openapi =
            get(root, "openapi", text).is_some() || get(root, "swagger", text).is_some();
        if !is_openapi {
            continue;
        }
        let base = base_path(root, text);
        let Some(paths) = get(root, "paths", text) else {
            continue;
        };
        for path_entry in entries(paths) {
            let path = path_entry.key_text(text);
            let Some(item) = path_entry.value else {
                continue;
            };
            if !path.starts_with('/') {
                continue;
            }
            let shared = get(item, "parameters", text)
                .map(|p| canonical(p, root, text, 0))
                .unwrap_or_default();
            for op_entry in entries(item) {
                let method = op_entry.key_text(text).to_ascii_lowercase();
                if !HTTP_METHODS.contains(&method.as_str()) {
                    continue;
                }
                let Some(operation) = op_entry.value else {
                    continue;
                };
                let part = |key: &str| {
                    get(operation, key, text)
                        .map(|n| canonical(n, root, text, 0))
                        .unwrap_or_default()
                };
                let hash = hash_hex(
                    "openapi-operation/v1",
                    &[
                        &shared,
                        &part("parameters"),
                        &part("requestBody"),
                        &part("responses"),
                    ],
                );
                let mut attrs = BTreeMap::new();
                attrs.insert(SCHEMA_HASH.to_owned(), hash);
                if let Some(id) = get(operation, "operationId", text).and_then(|n| scalar(n, text))
                {
                    attrs.insert(ATTR_OPERATION.to_owned(), id);
                }
                let Some(range) = tree::line(op_entry.key) else {
                    continue;
                };
                let key = format!("{} {base}{path}", method.to_ascii_uppercase());
                out.extend(ctx.extraction(
                    ContractKind::Endpoint,
                    Role::Definition,
                    &key,
                    range,
                    None,
                    EvidenceType::ContractDerived,
                    attrs,
                ));
            }
        }
    }
    out
}

/// Channels of an AsyncAPI 2 / 3 document (v3 `address` wins over the
/// channel id), with the hash of the channel's messages.
pub(crate) fn asyncapi(ctx: &Ctx<'_>, text: &str) -> Vec<Extraction> {
    let Some(language) = data_language(ctx.path) else {
        return Vec::new();
    };
    let Some(tree) = ctx.tree(language, text) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for root in document_roots(&tree) {
        if get(root, "asyncapi", text).is_none() {
            continue;
        }
        let Some(channels) = get(root, "channels", text) else {
            continue;
        };
        for entry in entries(channels) {
            let id = entry.key_text(text);
            let Some(channel) = entry.value else {
                continue;
            };
            let address = get(channel, "address", text)
                .and_then(|n| scalar(n, text))
                .filter(|a| !a.is_empty() && a != "null");
            let name = address.unwrap_or(id);
            let hash = hash_hex("asyncapi-channel/v1", &[&canonical(channel, root, text, 0)]);
            let mut attrs = BTreeMap::new();
            attrs.insert(SCHEMA_HASH.to_owned(), hash);
            let Some(range) = tree::line(entry.key) else {
                continue;
            };
            out.extend(ctx.extraction(
                ContractKind::Topic,
                Role::Definition,
                &name,
                range,
                None,
                EvidenceType::ContractDerived,
                attrs,
            ));
        }
    }
    out
}

/// Schema version from a file name or `$id` (`x.v2.json` -> `2`).
fn version_of(text: &str) -> Option<String> {
    let lower = text.to_ascii_lowercase();
    let mut found = None;
    for (index, _) in lower.match_indices(".v") {
        let digits: String = lower
            .get(index + 2..)
            .unwrap_or("")
            .chars()
            .take_while(char::is_ascii_digit)
            .collect();
        if !digits.is_empty() {
            found = Some(digits);
        }
    }
    found
}

/// A JSON Schema file describing one event: under an `events/` directory
/// (or named `*.schema.json`), with a `title` that names the topic.
pub(crate) fn event_schema(ctx: &Ctx<'_>, text: &str) -> Vec<Extraction> {
    let path = ctx.path.as_str();
    let in_events = ctx
        .path
        .components()
        .any(|c| matches!(c, "events" | "event-schemas" | "schemas"))
        || path.ends_with(".schema.json");
    if !in_events || data_language(ctx.path) != Some(knowell_parse::Language::Json) {
        return Vec::new();
    }
    let Some(tree) = ctx.tree(knowell_parse::Language::Json, text) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for root in document_roots(&tree) {
        let Some(title_node) = get(root, "title", text) else {
            continue;
        };
        let Some(title) = scalar(title_node, text) else {
            continue;
        };
        let topic_like = !title.is_empty()
            && !title.chars().any(char::is_whitespace)
            && title
                .chars()
                .any(|c| matches!(c, '.' | '-' | '_' | ':' | '/'));
        let has_schema =
            get(root, "$schema", text).is_some() || get(root, "properties", text).is_some();
        if !topic_like || !has_schema {
            continue;
        }
        let mut fields: Vec<String> = get(root, "properties", text)
            .map(|p| entries(p).iter().map(|e| e.key_text(text)).collect())
            .unwrap_or_default();
        fields.sort();
        fields.dedup();
        let shape = [
            get(root, "properties", text)
                .map(|n| canonical(n, root, text, 0))
                .unwrap_or_default(),
            get(root, "required", text)
                .map(|n| canonical(n, root, text, 0))
                .unwrap_or_default(),
        ];
        let mut attrs = BTreeMap::new();
        attrs.insert(
            SCHEMA_HASH.to_owned(),
            hash_hex("event-schema/v1", &[&shape[0], &shape[1]]),
        );
        attrs.insert(ATTR_FIELDS.to_owned(), fields.join(","));
        let id = get(root, "$id", text)
            .and_then(|n| scalar(n, text))
            .unwrap_or_default();
        if let Some(version) = version_of(ctx.path.file_name()).or_else(|| version_of(&id)) {
            attrs.insert(ATTR_VERSION.to_owned(), version);
        }
        let Some(range) = tree::line(title_node) else {
            continue;
        };
        out.extend(ctx.extraction(
            ContractKind::Topic,
            Role::Definition,
            &title,
            range,
            None,
            EvidenceType::ContractDerived,
            attrs,
        ));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn versions() {
        assert_eq!(
            version_of("subscription.cancelled.v1.json").as_deref(),
            Some("1")
        );
        assert_eq!(version_of("x.v12.json").as_deref(), Some("12"));
        assert_eq!(version_of("x.json"), None);
        assert_eq!(version_of("a.very.json"), None);
    }
}
