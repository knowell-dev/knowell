//! YAML / JSON / TOML structure: keys, and the contract symbols of known
//! dialects (OpenAPI endpoints and schemas, AsyncAPI channels and
//! operations, Compose services, Kubernetes resources).

use knowell_core::RepoPath;
use tree_sitter::{Node, Tree};

use crate::extract::{Budget, Draft};
use crate::language::{Dialect, Language, is_compose_name};
use crate::model::{Degradation, SymbolKind};
use crate::text::{bounded, one_line, slice, strip_quotes};

const HTTP_METHODS: &[&str] = &[
    "get", "put", "post", "delete", "options", "head", "patch", "trace",
];
const MAX_KEY_BYTES: usize = 200;
const MAX_VALUE_BYTES: usize = 120;
/// Second-level keys listed per map in plain config files.
const MAX_NESTED_KEYS: usize = 200;
/// Wrapper nodes unwrapped while looking for a value.
const MAX_UNWRAP: usize = 16;

pub(crate) struct Structured {
    pub(crate) drafts: Vec<Draft>,
    pub(crate) dialect: Option<Dialect>,
    pub(crate) degraded: Option<Degradation>,
}

struct Ctx<'a> {
    text: &'a str,
    drafts: Vec<Draft>,
    max: usize,
    budget: Budget<'a>,
    degraded: Option<Degradation>,
}

impl Ctx<'_> {
    /// Whether more drafts may be added; records why not.
    fn room(&mut self) -> bool {
        if self.degraded.is_some() {
            return false;
        }
        if self.drafts.len() >= self.max {
            self.degraded = Some(Degradation::Truncated { limit: self.max });
            return false;
        }
        if let Some(reason) = self.budget.exceeded() {
            self.degraded = Some(reason);
            return false;
        }
        true
    }

    fn push(&mut self, draft: Draft) {
        if self.room() {
            self.drafts.push(draft);
        }
    }
}

#[derive(Clone, Copy)]
struct Entry<'t> {
    pair: Node<'t>,
    key_node: Node<'t>,
    value: Option<Node<'t>>,
}

pub(crate) fn extract(
    tree: &Tree,
    text: &str,
    language: Language,
    path: &RepoPath,
    budget: Budget<'_>,
    max_symbols: usize,
) -> Structured {
    let mut ctx = Ctx {
        text,
        drafts: Vec::new(),
        max: max_symbols,
        budget,
        degraded: None,
    };
    let root = tree.root_node();
    let dialect = if language == Language::Toml {
        toml(root, &mut ctx);
        None
    } else {
        let documents = documents(root);
        let dialect = documents
            .first()
            .and_then(|doc| detect_dialect(*doc, text, language, path));
        for document in documents {
            match dialect {
                Some(Dialect::OpenApi) => openapi(document, &mut ctx),
                Some(Dialect::AsyncApi) => asyncapi(document, &mut ctx),
                Some(Dialect::Compose) => compose(document, &mut ctx),
                Some(Dialect::Kubernetes) => kubernetes(document, &mut ctx),
                None => plain(document, &mut ctx),
            }
        }
        dialect
    };
    Structured {
        drafts: ctx.drafts,
        dialect,
        degraded: ctx.degraded,
    }
}

/// Root values: one per YAML document, one for JSON.
fn documents(root: Node<'_>) -> Vec<Node<'_>> {
    let mut cursor = root.walk();
    let children: Vec<Node<'_>> = root.named_children(&mut cursor).collect();
    match root.kind() {
        "stream" => children
            .into_iter()
            .filter(|c| c.kind() == "document")
            .filter_map(unwrap)
            .collect(),
        _ => children
            .into_iter()
            .filter(|c| !c.kind().contains("comment"))
            .filter_map(unwrap)
            .take(1)
            .collect(),
    }
}

/// Descends through YAML wrapper nodes (document, block_node, flow_node,
/// anchors, tags) to the node that carries the value.
fn unwrap(node: Node<'_>) -> Option<Node<'_>> {
    let mut current = node;
    for _ in 0..MAX_UNWRAP {
        match current.kind() {
            "document" | "block_node" | "flow_node" => {
                let mut cursor = current.walk();
                let next = current
                    .named_children(&mut cursor)
                    .find(|c| !matches!(c.kind(), "anchor" | "tag" | "comment"));
                current = next?;
            }
            _ => return Some(current),
        }
    }
    None
}

fn entries<'t>(map: Node<'t>) -> Vec<Entry<'t>> {
    let pair_kind = match map.kind() {
        "block_mapping" => "block_mapping_pair",
        "flow_mapping" => "flow_pair",
        "object" => "pair",
        _ => return Vec::new(),
    };
    let mut cursor = map.walk();
    map.named_children(&mut cursor)
        .filter(|pair| pair.kind() == pair_kind)
        .filter_map(|pair| {
            Some(Entry {
                pair,
                key_node: pair.child_by_field_name("key")?,
                value: pair.child_by_field_name("value").and_then(unwrap),
            })
        })
        .collect()
}

fn key_text(entry: &Entry<'_>, text: &str) -> Option<String> {
    let key = scalar(entry.key_node, text)?;
    let key = one_line(&key);
    (!key.is_empty()).then(|| bounded(&key, MAX_KEY_BYTES))
}

/// The text of a scalar value, quotes removed.
fn scalar(node: Node<'_>, text: &str) -> Option<String> {
    let node = unwrap(node)?;
    let raw = slice(text, &node.byte_range());
    match node.kind() {
        "plain_scalar" | "number" | "true" | "false" | "null" => Some(raw.trim().to_owned()),
        "double_quote_scalar" | "string" => Some(
            strip_quotes(raw)
                .replace("\\\"", "\"")
                .replace("\\\\", "\\"),
        ),
        "single_quote_scalar" => Some(strip_quotes(raw).replace("''", "'")),
        "block_scalar" => {
            let body = raw.split_once('\n').map_or("", |(_, rest)| rest);
            Some(one_line(body))
        }
        _ => None,
    }
}

fn lookup<'t>(map: Option<Node<'t>>, key: &str, text: &str) -> Option<Node<'t>> {
    entries(map?)
        .into_iter()
        .find(|e| key_text(e, text).as_deref() == Some(key))
        .and_then(|e| e.value)
}

fn lookup_scalar(map: Option<Node<'_>>, key: &str, text: &str) -> Option<String> {
    lookup(map, key, text)
        .and_then(|value| scalar(value, text))
        .filter(|s| !s.is_empty())
}

fn detect_dialect(
    root: Node<'_>,
    text: &str,
    language: Language,
    path: &RepoPath,
) -> Option<Dialect> {
    let keys: Vec<String> = entries(root)
        .iter()
        .filter_map(|e| key_text(e, text))
        .collect();
    let has = |k: &str| keys.iter().any(|key| key == k);
    if has("openapi") || has("swagger") {
        Some(Dialect::OpenApi)
    } else if has("asyncapi") {
        Some(Dialect::AsyncApi)
    } else if language == Language::Yaml && is_compose_name(path) {
        Some(Dialect::Compose)
    } else if has("apiVersion") && has("kind") {
        Some(Dialect::Kubernetes)
    } else {
        None
    }
}

fn key_draft(entry: &Entry<'_>, key: String, kind: SymbolKind, ctx: &Ctx<'_>) -> Draft {
    let value = entry.value.and_then(|v| scalar(v, ctx.text));
    let signature = match value {
        Some(value) if !value.is_empty() => {
            format!("{key}: {}", bounded(&one_line(&value), MAX_VALUE_BYTES))
        }
        _ => format!("{key}:"),
    };
    let mut draft = Draft::simple(kind, key, entry.pair.byte_range(), signature);
    draft.name_start = entry.key_node.start_byte();
    draft
}

/// Plain config: top-level keys and their direct children.
fn plain(root: Node<'_>, ctx: &mut Ctx<'_>) {
    for entry in entries(root) {
        let Some(key) = key_text(&entry, ctx.text) else {
            continue;
        };
        let draft = key_draft(&entry, key, SymbolKind::Key, ctx);
        ctx.push(draft);
        if let Some(value) = entry.value {
            for child in entries(value).into_iter().take(MAX_NESTED_KEYS) {
                if let Some(child_key) = key_text(&child, ctx.text) {
                    let draft = key_draft(&child, child_key, SymbolKind::Key, ctx);
                    ctx.push(draft);
                }
            }
        }
        if !ctx.room() {
            return;
        }
    }
}

fn openapi(root: Node<'_>, ctx: &mut Ctx<'_>) {
    let text = ctx.text;
    for entry in entries(root) {
        let Some(key) = key_text(&entry, text) else {
            continue;
        };
        match key.as_str() {
            "paths" => {
                for path in entry.value.map(entries).unwrap_or_default() {
                    let Some(route) = key_text(&path, text) else {
                        continue;
                    };
                    for operation in path.value.map(entries).unwrap_or_default() {
                        let Some(method) = key_text(&operation, text) else {
                            continue;
                        };
                        if !HTTP_METHODS.contains(&method.to_ascii_lowercase().as_str()) {
                            continue;
                        }
                        let name = format!("{} {route}", method.to_ascii_uppercase());
                        ctx.push(operation_draft(
                            &operation,
                            name,
                            SymbolKind::Endpoint,
                            text,
                        ));
                    }
                }
            }
            "components" => {
                if let Some(schemas) = lookup(entry.value, "schemas", text) {
                    schema_drafts(schemas, ctx);
                }
            }
            "definitions" => {
                if let Some(definitions) = entry.value {
                    schema_drafts(definitions, ctx);
                }
            }
            _ => {
                let draft = key_draft(&entry, key, SymbolKind::Key, ctx);
                ctx.push(draft);
            }
        }
        if !ctx.room() {
            return;
        }
    }
}

/// An operation (OpenAPI method, AsyncAPI publish / subscribe / operation):
/// operationId in the signature, summary or description as doc.
fn operation_draft(operation: &Entry<'_>, name: String, kind: SymbolKind, text: &str) -> Draft {
    let body = operation.value;
    let operation_id = lookup_scalar(body, "operationId", text);
    let doc = lookup_scalar(body, "summary", text)
        .or_else(|| lookup_scalar(body, "description", text))
        .map(|d| bounded(&d, 2000));
    let signature = match &operation_id {
        Some(id) => format!("{name} (operationId: {id})"),
        None => name.clone(),
    };
    let mut draft = Draft::simple(kind, name, operation.pair.byte_range(), signature);
    draft.name_start = operation.key_node.start_byte();
    draft.doc = doc;
    draft
}

fn schema_drafts(schemas: Node<'_>, ctx: &mut Ctx<'_>) {
    let text = ctx.text;
    for schema in entries(schemas) {
        let Some(name) = key_text(&schema, text) else {
            continue;
        };
        let signature = match lookup_scalar(schema.value, "type", text) {
            Some(kind) => format!("schema {name}: {kind}"),
            None => format!("schema {name}"),
        };
        let mut draft = Draft::simple(
            SymbolKind::Schema,
            name,
            schema.pair.byte_range(),
            signature,
        );
        draft.name_start = schema.key_node.start_byte();
        draft.doc = lookup_scalar(schema.value, "description", text).map(|d| bounded(&d, 2000));
        ctx.push(draft);
    }
}

fn asyncapi(root: Node<'_>, ctx: &mut Ctx<'_>) {
    let text = ctx.text;
    for entry in entries(root) {
        let Some(key) = key_text(&entry, text) else {
            continue;
        };
        match key.as_str() {
            "channels" => {
                for channel in entry.value.map(entries).unwrap_or_default() {
                    let Some(channel_name) = key_text(&channel, text) else {
                        continue;
                    };
                    let address = lookup_scalar(channel.value, "address", text);
                    let signature = match &address {
                        Some(address) => format!("channel {channel_name} ({address})"),
                        None => format!("channel {channel_name}"),
                    };
                    let mut draft = Draft::simple(
                        SymbolKind::Channel,
                        channel_name.clone(),
                        channel.pair.byte_range(),
                        signature,
                    );
                    draft.name_start = channel.key_node.start_byte();
                    draft.doc = lookup_scalar(channel.value, "description", text);
                    ctx.push(draft);
                    // AsyncAPI 2: operations nested in the channel.
                    for operation in channel.value.map(entries).unwrap_or_default() {
                        let Some(action) = key_text(&operation, text) else {
                            continue;
                        };
                        if action == "publish" || action == "subscribe" {
                            let name = format!("{} {channel_name}", action.to_ascii_uppercase());
                            ctx.push(operation_draft(
                                &operation,
                                name,
                                SymbolKind::Endpoint,
                                text,
                            ));
                        }
                    }
                }
            }
            "operations" => {
                // AsyncAPI 3: top-level operations referencing channels.
                for operation in entry.value.map(entries).unwrap_or_default() {
                    let Some(id) = key_text(&operation, text) else {
                        continue;
                    };
                    let action = lookup_scalar(operation.value, "action", text)
                        .unwrap_or_else(|| "operation".to_owned());
                    let channel = lookup(operation.value, "channel", text)
                        .and_then(|c| lookup_scalar(Some(c), "$ref", text))
                        .and_then(|r| r.rsplit('/').next().map(str::to_owned))
                        .unwrap_or_else(|| id.clone());
                    let name = format!("{} {channel}", action.to_ascii_uppercase());
                    let mut draft =
                        operation_draft(&operation, name.clone(), SymbolKind::Endpoint, text);
                    draft.signature = Some(format!("{name} (operationId: {id})"));
                    ctx.push(draft);
                }
            }
            "components" => {
                if let Some(schemas) = lookup(entry.value, "schemas", text) {
                    schema_drafts(schemas, ctx);
                }
                if let Some(messages) = lookup(entry.value, "messages", text) {
                    for message in entries(messages) {
                        if let Some(name) = key_text(&message, text) {
                            let signature = format!("message {name}");
                            let mut draft = Draft::simple(
                                SymbolKind::Message,
                                name,
                                message.pair.byte_range(),
                                signature,
                            );
                            draft.name_start = message.key_node.start_byte();
                            ctx.push(draft);
                        }
                    }
                }
            }
            _ => {
                let draft = key_draft(&entry, key, SymbolKind::Key, ctx);
                ctx.push(draft);
            }
        }
        if !ctx.room() {
            return;
        }
    }
}

fn compose(root: Node<'_>, ctx: &mut Ctx<'_>) {
    let text = ctx.text;
    for entry in entries(root) {
        let Some(key) = key_text(&entry, text) else {
            continue;
        };
        let is_services = key == "services";
        let draft = key_draft(&entry, key, SymbolKind::Key, ctx);
        ctx.push(draft);
        if is_services {
            for service in entry.value.map(entries).unwrap_or_default() {
                let Some(name) = key_text(&service, text) else {
                    continue;
                };
                let signature = match lookup_scalar(service.value, "image", text) {
                    Some(image) => format!("service {name} (image: {image})"),
                    None => format!("service {name}"),
                };
                let mut draft = Draft::simple(
                    SymbolKind::Service,
                    name,
                    service.pair.byte_range(),
                    signature,
                );
                draft.name_start = service.key_node.start_byte();
                ctx.push(draft);
            }
        }
        if !ctx.room() {
            return;
        }
    }
}

fn kubernetes(root: Node<'_>, ctx: &mut Ctx<'_>) {
    let text = ctx.text;
    let kind = lookup_scalar(Some(root), "kind", text);
    let name =
        lookup(Some(root), "metadata", text).and_then(|m| lookup_scalar(Some(m), "name", text));
    match (kind, name) {
        (Some(kind), Some(name)) => {
            let full = format!("{kind}/{name}");
            let api = lookup_scalar(Some(root), "apiVersion", text).unwrap_or_default();
            let signature = format!("{full} ({api})");
            ctx.push(Draft::simple(
                SymbolKind::Resource,
                full,
                root.byte_range(),
                signature,
            ));
        }
        _ => plain(root, ctx),
    }
}

fn toml(root: Node<'_>, ctx: &mut Ctx<'_>) {
    let text = ctx.text;
    let mut cursor = root.walk();
    let children: Vec<Node<'_>> = root.named_children(&mut cursor).collect();
    for child in children {
        match child.kind() {
            "pair" => toml_pair(child, ctx),
            "table" | "table_array_element" => {
                let Some(header) = toml_header(child) else {
                    continue;
                };
                let name = toml_key(slice(text, &header.byte_range()));
                let signature = if child.kind() == "table" {
                    format!("[{name}]")
                } else {
                    format!("[[{name}]]")
                };
                let mut draft =
                    Draft::simple(SymbolKind::Section, name, child.byte_range(), signature);
                draft.name_start = header.start_byte();
                ctx.push(draft);
                let mut inner = child.walk();
                let pairs: Vec<Node<'_>> = child
                    .named_children(&mut inner)
                    .filter(|n| n.kind() == "pair")
                    .take(MAX_NESTED_KEYS)
                    .collect();
                for pair in pairs {
                    toml_pair(pair, ctx);
                }
            }
            _ => {}
        }
        if !ctx.room() {
            return;
        }
    }
}

fn toml_header(table: Node<'_>) -> Option<Node<'_>> {
    let mut cursor = table.walk();
    table
        .named_children(&mut cursor)
        .find(|n| matches!(n.kind(), "bare_key" | "dotted_key" | "quoted_key"))
}

fn toml_key(raw: &str) -> String {
    let joined: Vec<String> = raw
        .split('.')
        .map(|part| strip_quotes(part.trim()).to_owned())
        .collect();
    bounded(&joined.join("."), MAX_KEY_BYTES)
}

fn toml_pair(pair: Node<'_>, ctx: &mut Ctx<'_>) {
    let Some(key) = pair.named_child(0) else {
        return;
    };
    let name = toml_key(slice(ctx.text, &key.byte_range()));
    if name.is_empty() {
        return;
    }
    let raw = slice(ctx.text, &pair.byte_range());
    let first_line = raw.lines().next().unwrap_or("");
    let signature = bounded(&one_line(first_line), MAX_KEY_BYTES + MAX_VALUE_BYTES);
    let mut draft = Draft::simple(SymbolKind::Key, name, pair.byte_range(), signature);
    draft.name_start = key.start_byte();
    ctx.push(draft);
}
