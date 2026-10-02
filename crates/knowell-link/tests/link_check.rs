//! Linker and check behaviour on small synthetic workspaces built from
//! inline sources.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use std::collections::{BTreeMap, BTreeSet};

use knowell_core::{Name, RepoPath};
use knowell_graph::{ContractKind, EdgeKind, EvidenceType, NodeId, Resolution};
use knowell_link::{
    CheckOptions, ExtractOptions, Finding, LinkError, LinkOptions, LinkOutput, PackSet,
    ProjectExtractions, check, extract_project, link,
};

fn packs() -> &'static PackSet {
    static PACKS: std::sync::OnceLock<PackSet> = std::sync::OnceLock::new();
    PACKS.get_or_init(|| PackSet::builtin().unwrap())
}

fn extract(project: &str, files: &[(&str, &str)]) -> ProjectExtractions {
    let contents: BTreeMap<RepoPath, String> = files
        .iter()
        .map(|(p, c)| (RepoPath::new(*p).unwrap(), (*c).to_owned()))
        .collect();
    let paths: Vec<RepoPath> = contents.keys().cloned().collect();
    extract_project(
        &Name::new(project).unwrap(),
        &paths,
        packs(),
        &ExtractOptions::default(),
        &mut |path| contents.get(path).cloned(),
    )
}

fn edge_between(
    linked: &LinkOutput,
    from_project: &str,
    from_key: &str,
    to: &NodeId,
    kind: EdgeKind,
) -> Option<knowell_graph::Edge> {
    let from = NodeId::symbol(&Name::new(from_project).unwrap(), from_key);
    linked
        .edges()
        .values()
        .flat_map(|e| e.values())
        .find(|r| r.from == from && &r.to == to && r.edge.kind == kind)
        .map(|r| r.edge.clone())
}

fn codes(findings: &[Finding]) -> Vec<(String, String)> {
    findings
        .iter()
        .map(|f| {
            (
                f.code.clone(),
                f.subject
                    .as_ref()
                    .map(|s| s.to_string())
                    .unwrap_or_default(),
            )
        })
        .collect()
}

const API: &str = r#"from fastapi import APIRouter

router = APIRouter(prefix="/v1/items")


@router.get("")
def list_items():
    return []


@router.get("/{item_id}")
def get_item(item_id: str):
    return {}


@router.post("")
def create_item():
    return {}


@router.delete("/{item_id}/archive")
def archive_item(item_id: str):
    return None
"#;

const SPEC: &str = r#"openapi: 3.0.3
info: { title: Items, version: 1.0.0 }
paths:
  /v1/items:
    get: { operationId: listItems, responses: { "200": { description: ok } } }
    post: { operationId: createItem, responses: { "201": { description: ok } } }
  /v1/items/{id}:
    get: { operationId: getItem, responses: { "200": { description: ok } } }
  /v1/items/export:
    get: { operationId: exportItems, responses: { "200": { description: ok } } }
"#;

const WEB: &str = r#"export const listItems = () => fetch("/api/v1/items");
export const getItem = (id: string) => fetch(`/v1/items/${id}`);
export const createItem = () => fetch("/v1/items", { method: "POST" });
export const legacy = () => fetch("/v1/legacy/report");
export const external = () => fetch("https://maps.example.com/v2/geocode");
export const anything = (url: string) => fetch(url);
"#;

fn items_workspace(options: &LinkOptions) -> (Vec<ProjectExtractions>, LinkOutput, Vec<Finding>) {
    let projects = vec![
        extract(
            "items-api",
            &[
                (
                    "pyproject.toml",
                    "[project]\nname = \"x\"\ndependencies = [\"fastapi\"]\n",
                ),
                ("app/routes.py", API),
            ],
        ),
        extract("contracts", &[("openapi/items-api.yaml", SPEC)]),
        extract("web", &[("src/api.ts", WEB)]),
    ];
    let linked = link(&projects, options).unwrap();
    let findings = check(&projects, &linked, &CheckOptions::default()).unwrap();
    (projects, linked, findings)
}

#[test]
fn openapi_coverage_works_in_both_directions() {
    let (_, _, findings) = items_workspace(&LinkOptions::default());
    let found = codes(&findings);
    assert!(found.contains(&(
        "link.endpoint_undocumented".into(),
        "contract:endpoint:DELETE /v1/items/{}/archive".into()
    )));
    assert!(found.contains(&(
        "link.endpoint_unimplemented".into(),
        "contract:endpoint:GET /v1/items/export".into()
    )));
    // Documented and served endpoints are neither.
    assert!(
        !found
            .iter()
            .any(|(code, subject)| code.starts_with("link.endpoint_un")
                && subject.ends_with("POST /v1/items"))
    );
}

#[test]
fn clients_link_exactly_by_pattern_or_not_at_all() {
    let (_, linked, findings) = items_workspace(&LinkOptions::default());
    let get = NodeId::contract(ContractKind::Endpoint, "GET /v1/items/{}");
    let edge = edge_between(
        &linked,
        "web",
        "src/api.ts#getItem",
        &get,
        EdgeKind::Consumes,
    )
    .unwrap();
    assert_eq!(edge.evidence, EvidenceType::Heuristic);
    assert_eq!(edge.resolution, Resolution::Resolved);
    let post = NodeId::contract(ContractKind::Endpoint, "POST /v1/items");
    let edge = edge_between(
        &linked,
        "web",
        "src/api.ts#createItem",
        &post,
        EdgeKind::Consumes,
    )
    .unwrap();
    // A literal path equal to a route documented in OpenAPI.
    assert_eq!(edge.evidence, EvidenceType::ContractDerived);

    // `/api/v1/items` has no exact match without the prefix option.
    let list = NodeId::contract(ContractKind::Endpoint, "GET /v1/items");
    assert!(
        edge_between(
            &linked,
            "web",
            "src/api.ts#listItems",
            &list,
            EdgeKind::Consumes
        )
        .is_none()
    );

    let found = codes(&findings);
    assert!(found.contains(&(
        "link.endpoint_without_provider".into(),
        "contract:endpoint:GET /v1/legacy/report".into()
    )));
    assert!(found.contains(&(
        "link.unresolved_reference".into(),
        "contract:endpoint:GET {}".into()
    )));
    let unresolved = findings
        .iter()
        .find(|f| f.code == "link.unresolved_reference")
        .unwrap();
    assert!(
        unresolved.message.contains("only known at run time"),
        "{}",
        unresolved.message
    );
}

#[test]
fn path_prefixes_and_external_contracts() {
    let mut options = LinkOptions {
        path_prefixes: vec!["/api".into()],
        ..LinkOptions::default()
    };
    options
        .external
        .insert((ContractKind::Endpoint, "GET /v2/geocode".into()));
    let (_, linked, findings) = items_workspace(&options);
    let list = NodeId::contract(ContractKind::Endpoint, "GET /v1/items");
    let edge = edge_between(
        &linked,
        "web",
        "src/api.ts#listItems",
        &list,
        EdgeKind::Consumes,
    )
    .unwrap();
    assert_eq!(edge.evidence, EvidenceType::Heuristic);
    assert_eq!(edge.attr("match"), Some("prefix"));
    let geocode = linked
        .contracts()
        .get(&NodeId::contract(ContractKind::Endpoint, "GET /v2/geocode"))
        .unwrap();
    assert_eq!(geocode.attr("external"), Some("true"));
    assert!(
        !codes(&findings)
            .iter()
            .any(|(_, s)| s.ends_with("/v2/geocode"))
    );
}

#[test]
fn relinking_removes_edges_that_disappeared() {
    let (projects, first, _) = items_workspace(&LinkOptions::default());
    let generations = |g: u64| -> BTreeMap<Name, u64> {
        projects.iter().map(|p| (p.project.clone(), g)).collect()
    };
    let mut graph = knowell_graph::CodeGraph::new();
    for delta in first.deltas(&generations(1), None).unwrap() {
        graph.apply(delta).unwrap();
    }
    // The web client stops calling `/v1/legacy/report`.
    let mut changed = projects.clone();
    let web = changed
        .iter_mut()
        .find(|p| p.project.as_str() == "web")
        .unwrap();
    web.extractions.retain(|e| !e.key.contains("legacy"));
    let second = link(&changed, &LinkOptions::default()).unwrap();
    let legacy = NodeId::contract(ContractKind::Endpoint, "GET /v1/legacy/report");
    let before = graph.edges().filter(|(k, _)| k.to == legacy).count();
    assert_eq!(before, 1);
    for delta in second
        .replacing_deltas(&first, &generations(2), &graph)
        .unwrap()
    {
        graph.apply(delta).unwrap();
    }
    assert_eq!(graph.edges().filter(|(k, _)| k.to == legacy).count(), 0);
    // Everything else is still linked.
    let post = NodeId::contract(ContractKind::Endpoint, "POST /v1/items");
    assert!(
        graph
            .edges()
            .any(|(k, _)| k.to == post && k.kind == EdgeKind::Consumes)
    );
}

#[test]
fn duplicate_projects_are_rejected() {
    let project = extract("web", &[("src/api.ts", WEB)]);
    let err = link(&[project.clone(), project], &LinkOptions::default()).unwrap_err();
    assert_eq!(err, LinkError::DuplicateProject(Name::new("web").unwrap()));
}

#[test]
fn consumer_fields_are_checked_against_the_event_schema() {
    let schema = r#"{
  "$schema": "https://json-schema.org/draft/2020-12/schema",
  "title": "order.shipped",
  "type": "object",
  "properties": { "orderId": { "type": "string" }, "shippedAt": { "type": "string" } }
}"#;
    let producer = "import { Kafka } from \"kafkajs\";\nconst producer = new Kafka({ brokers: [] }).producer();\nexport async function ship() {\n  await producer.send({ topic: \"order.shipped\", messages: [] });\n}\n";
    let consumer = "package events\n\nimport \"github.com/segmentio/kafka-go\"\n\ntype orderShipped struct {\n\tOrderID string `json:\"orderId\"`\n\tCarrier string `json:\"carrier\"`\n}\n\nfunc Reader() *kafka.Reader {\n\treturn kafka.NewReader(kafka.ReaderConfig{Topic: \"order.shipped\"})\n}\n";
    let projects = vec![
        extract("schemas", &[("events/order.shipped.v1.json", schema)]),
        extract("shop", &[("src/ship.ts", producer)]),
        extract("fulfilment", &[("internal/events/shipped.go", consumer)]),
    ];
    let linked = link(&projects, &LinkOptions::default()).unwrap();
    let findings = check(&projects, &linked, &CheckOptions::default()).unwrap();
    let mismatch = findings
        .iter()
        .find(|f| f.code == "link.consumer_field_mismatch")
        .expect("field mismatch not reported");
    assert!(
        mismatch.message.contains("`Carrier`"),
        "{}",
        mismatch.message
    );
    assert!(
        !mismatch.message.contains("OrderID"),
        "{}",
        mismatch.message
    );
    assert_eq!(
        mismatch.project.as_ref().map(Name::as_str),
        Some("fulfilment")
    );
    // The producer and consumer are linked through the defined topic.
    let topic = NodeId::contract(ContractKind::Topic, "order.shipped");
    assert!(
        edge_between(
            &linked,
            "shop",
            "src/ship.ts#ship",
            &topic,
            EdgeKind::Produces
        )
        .is_some()
    );
}

#[test]
fn i18n_keys_missing_in_a_locale_are_reported() {
    let page = "import { useTranslation } from \"react-i18next\";\nexport function Cart() {\n  const { t } = useTranslation();\n  return [t(\"cart.title\"), t(\"cart.empty\")];\n}\n";
    let projects = vec![extract(
        "web",
        &[
            ("src/Cart.tsx", page),
            (
                "src/locales/en.json",
                "{ \"cart\": { \"title\": \"Cart\", \"empty\": \"Empty\" } }",
            ),
            (
                "src/locales/de.json",
                "{ \"cart\": { \"title\": \"Warenkorb\" } }",
            ),
        ],
    )];
    let linked = link(&projects, &LinkOptions::default()).unwrap();
    let findings = check(&projects, &linked, &CheckOptions::default()).unwrap();
    let missing = findings
        .iter()
        .find(|f| f.code == "graph.i18n_key_missing_locale")
        .expect("missing locale not reported");
    assert_eq!(
        missing.subject.as_ref().map(NodeId::as_str),
        Some("contract:i18n_key:cart.empty")
    );
    assert!(missing.message.contains("de"), "{}", missing.message);
    assert_eq!(findings.len(), 1, "{findings:#?}");
}

#[test]
fn migration_entity_mismatches_cover_unknown_missing_and_dropped() {
    let migrations = [
        (
            "migrations/0001_items.sql",
            "CREATE TABLE items (id uuid PRIMARY KEY, name text NOT NULL);\nCREATE TABLE legacy (id int);\n",
        ),
        (
            "migrations/0002_price.sql",
            "ALTER TABLE items ADD COLUMN price integer;\nDROP TABLE legacy;\n",
        ),
    ];
    let models = r#"from sqlalchemy import Column, String
from sqlalchemy.orm import DeclarativeBase


class Base(DeclarativeBase):
    pass


class Item(Base):
    __tablename__ = "items"

    id = Column(String, primary_key=True)
    name = Column(String)
    colour = Column(String)


class Legacy(Base):
    __tablename__ = "legacy"

    id = Column(String, primary_key=True)
"#;
    let projects = vec![
        extract("db", &migrations),
        extract(
            "svc",
            &[
                (
                    "pyproject.toml",
                    "[project]\nname = \"svc\"\ndependencies = [\"sqlalchemy\"]\n",
                ),
                ("svc/models.py", models),
            ],
        ),
    ];
    let linked = link(&projects, &LinkOptions::default()).unwrap();
    let schema = linked.tables().get("items").unwrap();
    assert_eq!(schema.versions.len(), 2);
    assert!(schema.added.contains_key("price"));
    assert!(linked.tables().get("legacy").unwrap().dropped);
    let findings = check(&projects, &linked, &CheckOptions::default()).unwrap();
    let mismatches: Vec<&Finding> = findings
        .iter()
        .filter(|f| f.code == "link.migration_entity_mismatch")
        .collect();
    assert_eq!(mismatches.len(), 2, "{mismatches:#?}");
    let item = mismatches
        .iter()
        .find(|f| f.message.contains("`Item`"))
        .unwrap();
    assert!(
        item.message.contains("misses column(s) `price`"),
        "{}",
        item.message
    );
    assert!(
        item.message.contains("maps column(s) `colour`"),
        "{}",
        item.message
    );
    assert!(
        item.locations
            .iter()
            .any(|l| l.path.as_str() == "migrations/0002_price.sql")
    );
    let legacy = mismatches
        .iter()
        .find(|f| f.message.contains("`Legacy`"))
        .unwrap();
    assert!(legacy.message.contains("drop"), "{}", legacy.message);
    // Drift: the entity matches no version, so its edge hash differs.
    assert!(findings.iter().any(|f| f.code == "graph.contract_drift"));
}

#[test]
fn check_codes_can_be_filtered() {
    let (projects, linked, _) = items_workspace(&LinkOptions::default());
    let options = CheckOptions {
        codes: BTreeSet::from(["link.endpoint_unimplemented".to_owned()]),
        ..CheckOptions::default()
    };
    let findings = check(&projects, &linked, &options).unwrap();
    assert!(!findings.is_empty());
    assert!(
        findings
            .iter()
            .all(|f| f.code == "link.endpoint_unimplemented")
    );
}

#[test]
fn excluded_files_are_never_read_and_secrets_are_redacted() {
    let key = format!("AKIA{}", "FAKEFAKEFAKEFAKE");
    let canary = format!("KNOWELL_CANARY_{}", "0123456789abcdef");
    let files: BTreeMap<RepoPath, String> = [
        (".env", format!("TOKEN={canary}\n")),
        ("config/.env.production", format!("TOKEN={canary}\n")),
        ("deploy/id_rsa", "-----BEGIN FAKE-----".to_owned()),
        (
            "src/client.ts",
            format!("export const ACCESS_KEY = \"{key}\";\nexport const load = () => fetch(\"/v1/a\", {{ headers: {{ k: ACCESS_KEY }} }});\n"),
        ),
        (
            "docker-compose.yml",
            format!("services:\n  app:\n    image: x\n    environment:\n      TOKEN: {canary}\n      - BROKEN\n"),
        ),
    ]
    .into_iter()
    .map(|(p, c)| (RepoPath::new(p).unwrap(), c))
    .collect();
    let paths: Vec<RepoPath> = files.keys().cloned().collect();
    let mut read = Vec::new();
    let result = extract_project(
        &Name::new("app").unwrap(),
        &paths,
        packs(),
        &ExtractOptions::default(),
        &mut |path| {
            read.push(path.to_string());
            files.get(path).cloned()
        },
    );
    assert!(
        !read
            .iter()
            .any(|p| p.contains(".env") || p.contains("id_rsa")),
        "{read:?}"
    );
    assert_eq!(result.skipped.len(), 3, "{:?}", result.skipped);
    let text = format!("{result:?}");
    assert!(!text.contains(&key));
    assert!(!text.contains(&canary));
    let linked = link(std::slice::from_ref(&result), &LinkOptions::default()).unwrap();
    let findings = check(&[result], &linked, &CheckOptions::default()).unwrap();
    let all = format!("{linked:?}{findings:?}");
    assert!(!all.contains(&key) && !all.contains(&canary));
}

#[test]
fn hostile_and_malformed_files_do_not_panic() {
    let deep = format!("{}1{}", "[".repeat(5_000), "]".repeat(5_000));
    let huge = "a".repeat(3 * 1024 * 1024);
    let minified = format!("var a={};", "1+".repeat(20_000));
    let files: Vec<(&str, &str)> = vec![
        (
            "openapi.yaml",
            "openapi: 3\npaths:\n  /x:\n    get: [unterminated",
        ),
        (
            "api.json",
            "{\"openapi\": \"3.0.0\", \"paths\": {\"/x\": {\"get\": ",
        ),
        (
            "events/x.y.v1.json",
            "{\"title\": \"x.y\", \"properties\": ",
        ),
        (
            "svc.proto",
            "syntax = \"proto3\"; service S { rpc A(B) returns (",
        ),
        ("schema.prisma", "model A {\n  id String @map(\"\n"),
        ("deep.json", &deep),
        ("big.ts", &huge),
        ("min.js", &minified),
        (
            "bad.sql",
            "CREATE TABLE ((((; ALTER TABLE t ADD; DROP TABLE",
        ),
        (
            "k8s.yaml",
            "apiVersion: v1\nkind: Deployment\nspec: {containers: [{env: [{name: }]}]}",
        ),
        ("locales/en.json", "{\"a\": {\"b\": "),
        ("app_en.arb", "{\"@@locale\": 5, \"x\": "),
        ("binary.go", "package x\n\u{0}\u{1}\u{2}func (\u{fffd}"),
        ("crlf.py", "import os\r\nX = os.getenv(\"CRLF_NAME\")\r\n"),
    ];
    let result = extract("hostile", &files);
    assert!(result.skipped.iter().any(|s| s.path.as_str() == "big.ts"));
    // CRLF line endings still work.
    assert!(result.extractions.iter().any(|e| e.key == "CRLF_NAME"));
    let linked = link(std::slice::from_ref(&result), &LinkOptions::default()).unwrap();
    check(&[result], &linked, &CheckOptions::default()).unwrap();
}
