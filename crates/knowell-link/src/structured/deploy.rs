//! Deployment configuration: docker-compose and Kubernetes manifests.
//!
//! Environment variables are recorded by **name only**: the extractors read
//! the key (or the text before `=` in list form) and never decode, copy or
//! keep a value.

use std::collections::BTreeMap;

use knowell_graph::{ContractKind, EvidenceType};
use knowell_parse::Language;
use knowell_parse::tree_sitter::Node;

use super::tree::{self, entries, get, items, scalar};
use super::{Ctx, data_language};
use crate::model::{Extraction, Role};

/// Attribute: compose service declaring an env name.
const ATTR_SERVICE: &str = "service";
/// Attribute: Kubernetes workload (`metadata.name`).
const ATTR_WORKLOAD: &str = "workload";
/// Attribute: container name.
const ATTR_CONTAINER: &str = "container";
/// Attribute: container image.
const ATTR_IMAGE: &str = "image";
/// Attribute: comma-separated container ports.
const ATTR_PORTS: &str = "ports";
/// Attribute: where the infra definition comes from (`compose`, `kubernetes`).
const ATTR_SOURCE: &str = "source";

fn yaml_roots<'t>(tree: &'t knowell_parse::tree_sitter::Tree) -> Vec<Node<'t>> {
    tree::roots(tree.root_node())
        .into_iter()
        .filter(|n| tree::is_map(*n))
        .collect()
}

fn is_compose_name(file_name: &str) -> bool {
    let name = file_name.to_ascii_lowercase();
    let Some(stem) = name
        .strip_suffix(".yaml")
        .or_else(|| name.strip_suffix(".yml"))
    else {
        return false;
    };
    stem == "compose"
        || stem == "docker-compose"
        || stem.starts_with("compose.")
        || stem.starts_with("docker-compose.")
        || stem.starts_with("docker-compose-")
}

/// The `services` mapping of a compose file.
fn compose_services<'t>(ctx: &Ctx<'_>, root: Node<'t>, text: &str) -> Option<Node<'t>> {
    let services = get(root, "services", text)?;
    if !tree::is_map(services) {
        return None;
    }
    let looks_like_compose = is_compose_name(ctx.path.file_name())
        || entries(services).iter().any(|e| {
            e.value
                .is_some_and(|v| get(v, "image", text).is_some() || get(v, "build", text).is_some())
        });
    looks_like_compose.then_some(services)
}

/// The name in a list-form environment entry (`NAME=value` or `NAME`):
/// read from the source up to `=`, so the value is never decoded.
fn list_env_name(node: Node<'_>, text: &str) -> Option<String> {
    let raw = text.get(tree::unwrap(node).byte_range())?;
    let raw = raw.trim_start_matches(['"', '\'']);
    let name: String = raw
        .chars()
        .take_while(|c| *c != '=' && *c != '"' && *c != '\'' && !c.is_whitespace())
        .collect();
    (!name.is_empty()).then_some(name)
}

/// Env names declared under `services.*.environment` of compose files.
pub(crate) fn compose_env(ctx: &Ctx<'_>, text: &str) -> Vec<Extraction> {
    if data_language(ctx.path) != Some(Language::Yaml) {
        return Vec::new();
    }
    let Some(tree) = ctx.tree(Language::Yaml, text) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for root in yaml_roots(&tree) {
        let Some(services) = compose_services(ctx, root, text) else {
            continue;
        };
        for service in entries(services) {
            let service_name = service.key_text(text);
            let Some(environment) = service.value.and_then(|v| get(v, "environment", text)) else {
                continue;
            };
            let mut names: Vec<(String, Node<'_>)> = Vec::new();
            if tree::is_map(environment) {
                for entry in entries(environment) {
                    names.push((entry.key_text(text), entry.key));
                }
            } else {
                for item in items(environment) {
                    if let Some(name) = list_env_name(item, text) {
                        names.push((name, item));
                    }
                }
            }
            for (name, node) in names {
                let Some(range) = tree::line(node) else {
                    continue;
                };
                let mut attrs = BTreeMap::new();
                attrs.insert(ATTR_SERVICE.to_owned(), service_name.clone());
                out.extend(ctx.extraction(
                    ContractKind::EnvName,
                    Role::Definition,
                    &name,
                    range,
                    None,
                    EvidenceType::Syntactic,
                    attrs,
                ));
            }
        }
    }
    out
}

/// Container specs anywhere under a Kubernetes object.
fn containers<'t>(node: Node<'t>, text: &str, depth: usize, out: &mut Vec<Node<'t>>) {
    if depth > 12 {
        return;
    }
    for entry in entries(node) {
        let key = entry.key_text(text);
        let Some(value) = entry.value else {
            continue;
        };
        if matches!(key.as_str(), "containers" | "initContainers") {
            out.extend(items(value).into_iter().filter(|i| tree::is_map(*i)));
        } else if tree::is_map(value) {
            containers(value, text, depth + 1, out);
        }
    }
}

fn kubernetes_object<'t>(root: Node<'t>, text: &str) -> Option<(String, String)> {
    get(root, "apiVersion", text)?;
    let kind = get(root, "kind", text).and_then(|k| scalar(k, text))?;
    let name = get(root, "metadata", text)
        .and_then(|m| get(m, "name", text))
        .and_then(|n| scalar(n, text))
        .unwrap_or_default();
    Some((kind, name))
}

/// Env names declared in Kubernetes container `env` lists.
pub(crate) fn kubernetes_env(ctx: &Ctx<'_>, text: &str) -> Vec<Extraction> {
    if data_language(ctx.path) != Some(Language::Yaml) {
        return Vec::new();
    }
    let Some(tree) = ctx.tree(Language::Yaml, text) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for root in yaml_roots(&tree) {
        let Some((_, workload)) = kubernetes_object(root, text) else {
            continue;
        };
        let mut found = Vec::new();
        containers(root, text, 0, &mut found);
        for container in found {
            let container_name = get(container, "name", text)
                .and_then(|n| scalar(n, text))
                .unwrap_or_default();
            let Some(env) = get(container, "env", text) else {
                continue;
            };
            for item in items(env) {
                let Some(name_node) = get(item, "name", text) else {
                    continue;
                };
                let Some(name) = scalar(name_node, text) else {
                    continue;
                };
                let Some(range) = tree::line(name_node) else {
                    continue;
                };
                let mut attrs = BTreeMap::new();
                attrs.insert(ATTR_WORKLOAD.to_owned(), workload.clone());
                attrs.insert(ATTR_CONTAINER.to_owned(), container_name.clone());
                out.extend(ctx.extraction(
                    ContractKind::EnvName,
                    Role::Definition,
                    &name,
                    range,
                    None,
                    EvidenceType::Syntactic,
                    attrs,
                ));
            }
        }
    }
    out
}

fn container_port(raw: &str) -> String {
    let raw = raw.split('/').next().unwrap_or(raw);
    raw.rsplit(':').next().unwrap_or(raw).trim().to_owned()
}

fn join_ports(mut ports: Vec<String>) -> String {
    ports.retain(|p| !p.is_empty());
    ports.sort_by(|a, b| {
        (a.parse::<u32>().unwrap_or(u32::MAX), a).cmp(&(b.parse::<u32>().unwrap_or(u32::MAX), b))
    });
    ports.dedup();
    ports.join(",")
}

/// Services of compose files and Kubernetes workloads / services, with
/// their images and container ports.
pub(crate) fn infra(ctx: &Ctx<'_>, text: &str) -> Vec<Extraction> {
    if data_language(ctx.path) != Some(Language::Yaml) {
        return Vec::new();
    }
    let Some(tree) = ctx.tree(Language::Yaml, text) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for root in yaml_roots(&tree) {
        if let Some(services) = compose_services(ctx, root, text) {
            for service in entries(services) {
                let Some(definition) = service.value else {
                    continue;
                };
                let mut attrs = BTreeMap::new();
                attrs.insert(ATTR_SOURCE.to_owned(), "compose".to_owned());
                if let Some(image) = get(definition, "image", text).and_then(|n| scalar(n, text)) {
                    attrs.insert(ATTR_IMAGE.to_owned(), image);
                }
                let mut ports = Vec::new();
                for key in ["ports", "expose"] {
                    for item in get(definition, key, text).map(items).unwrap_or_default() {
                        if tree::is_map(item) {
                            if let Some(target) =
                                get(item, "target", text).and_then(|n| scalar(n, text))
                            {
                                ports.push(target);
                            }
                        } else if let Some(raw) = scalar(item, text) {
                            ports.push(container_port(&raw));
                        }
                    }
                }
                let ports = join_ports(ports);
                if !ports.is_empty() {
                    attrs.insert(ATTR_PORTS.to_owned(), ports);
                }
                let Some(range) = tree::line(service.key) else {
                    continue;
                };
                out.extend(ctx.extraction(
                    ContractKind::Infra,
                    Role::Definition,
                    &service.key_text(text),
                    range,
                    None,
                    EvidenceType::Syntactic,
                    attrs,
                ));
            }
            continue;
        }
        let Some((kind, name)) = kubernetes_object(root, text) else {
            continue;
        };
        if name.is_empty() {
            continue;
        }
        let mut ports = Vec::new();
        match kind.as_str() {
            "Service" => {
                let spec_ports = get(root, "spec", text)
                    .and_then(|s| get(s, "ports", text))
                    .map(items)
                    .unwrap_or_default();
                for port in spec_ports {
                    if let Some(value) = get(port, "port", text).and_then(|n| scalar(n, text)) {
                        ports.push(value);
                    }
                }
            }
            "Deployment" | "StatefulSet" | "DaemonSet" | "Job" | "CronJob" => {
                let mut found = Vec::new();
                containers(root, text, 0, &mut found);
                for container in found {
                    for port in get(container, "ports", text).map(items).unwrap_or_default() {
                        if let Some(value) =
                            get(port, "containerPort", text).and_then(|n| scalar(n, text))
                        {
                            ports.push(value);
                        }
                    }
                }
            }
            _ => continue,
        }
        let mut attrs = BTreeMap::new();
        attrs.insert(ATTR_SOURCE.to_owned(), format!("kubernetes:{kind}"));
        let ports = join_ports(ports);
        if !ports.is_empty() {
            attrs.insert(ATTR_PORTS.to_owned(), ports);
        }
        let Some(range) = tree::line(root) else {
            continue;
        };
        out.extend(ctx.extraction(
            ContractKind::Infra,
            Role::Definition,
            &name,
            range,
            None,
            EvidenceType::Syntactic,
            attrs,
        ));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_and_ports() {
        assert!(is_compose_name("docker-compose.yml"));
        assert!(is_compose_name("compose.prod.yaml"));
        assert!(!is_compose_name("values.yaml"));
        assert_eq!(container_port("80:8080"), "8080");
        assert_eq!(container_port("127.0.0.1:5432:5432/tcp"), "5432");
        assert_eq!(container_port("6379"), "6379");
        assert_eq!(
            join_ports(vec!["9090".into(), "80".into(), "80".into(), String::new()]),
            "80,9090"
        );
    }
}
