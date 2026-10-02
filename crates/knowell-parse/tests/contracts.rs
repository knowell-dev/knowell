//! Contract and structure files: SQL, protobuf, OpenAPI / AsyncAPI, Markdown,
//! YAML / JSON / TOML, Dockerfile, Compose, Kubernetes.

mod common;

use common::*;
use knowell_parse::{ChunkKind, ChunkOptions, Dialect, Language, SymbolKind as K, Tier};
use pretty_assertions::assert_eq;

const MIGRATION: &str = r#"-- Create subscriptions.
CREATE TABLE IF NOT EXISTS public.subscriptions (
    id BIGSERIAL PRIMARY KEY,
    customer_id BIGINT NOT NULL REFERENCES customers(id),
    plan TEXT NOT NULL DEFAULT 'free',
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    CONSTRAINT plan_check CHECK (plan <> '')
);

CREATE INDEX subscriptions_customer_idx ON subscriptions (customer_id);

ALTER TABLE subscriptions ADD COLUMN cancelled_at TIMESTAMPTZ;
ALTER TABLE subscriptions ADD COLUMN IF NOT EXISTS reason TEXT, ADD COLUMN note TEXT;

CREATE VIEW active_subscriptions AS SELECT * FROM subscriptions WHERE cancelled_at IS NULL;

INSERT INTO plans (name) VALUES ('free');
"#;

#[test]
fn sql_migration() {
    let file = parsed("db/migrations/0001_subscriptions.sql", MIGRATION);
    assert_eq!((file.language, file.tier), (Language::Sql, Tier::Contract));
    assert!(!file.has_errors);
    expect_symbols(
        &file,
        &[
            (K::Table, "subscriptions", 1, 8),
            (K::Column, "subscriptions.id", 3, 3),
            (K::Column, "subscriptions.customer_id", 4, 4),
            (K::Column, "subscriptions.plan", 5, 5),
            (K::Column, "subscriptions.created_at", 6, 6),
            (K::Index, "subscriptions_customer_idx", 10, 10),
            (K::Column, "subscriptions.cancelled_at", 12, 12),
            (K::Column, "subscriptions.reason", 13, 13),
            (K::Column, "subscriptions.note", 13, 13),
            (K::View, "active_subscriptions", 15, 15),
        ],
    );
    let table = symbol(&file, K::Table, "subscriptions");
    assert_eq!(
        table.signature,
        "CREATE TABLE IF NOT EXISTS public.subscriptions"
    );
    assert_eq!(table.doc.as_deref(), Some("Create subscriptions."));
    assert_eq!(
        symbol(&file, K::Column, "subscriptions.customer_id").signature,
        "customer_id BIGINT NOT NULL REFERENCES customers(id)"
    );
    let blocks: Vec<(u32, u32, Option<&str>)> = file
        .blocks
        .iter()
        .map(|b| (b.range.start(), b.range.end(), b.subject.as_deref()))
        .collect();
    assert_eq!(
        blocks,
        [
            (1, 8, Some("subscriptions")),
            (10, 10, Some("subscriptions")),
            (12, 12, Some("subscriptions")),
            (13, 13, Some("subscriptions")),
            (15, 15, Some("active_subscriptions")),
            (17, 17, Some("plans")),
        ]
    );
    assert!(file.blocks[0].byte_range.end == MIGRATION.find(");").unwrap() + 2);

    // One chunk per statement when nothing is grouped.
    let options = ChunkOptions {
        target_chars: 400,
        min_chars: 10,
        overlap_chars: 0,
    };
    let list = chunked(&file, MIGRATION, &options);
    assert_covers(MIGRATION, &list);
    assert_eq!(list.len(), 6);
    assert!(list.iter().all(|c| c.kind == ChunkKind::Statement));
    assert_eq!(list[0].symbol_path.as_deref(), Some("subscriptions"));
    assert!(
        list[0]
            .text
            .starts_with("-- Create subscriptions.\nCREATE TABLE")
    );
    assert!(list[0].text.ends_with(");"));
    assert_eq!(list[5].symbol_path.as_deref(), Some("plans"));

    let skeleton = skeleton_of(&file, MIGRATION);
    assert!(skeleton.starts_with(
        "-- Create subscriptions.\nCREATE TABLE IF NOT EXISTS public.subscriptions (\n    id BIGSERIAL PRIMARY KEY,\n"
    ), "{skeleton}");
    assert!(skeleton.contains("ALTER TABLE subscriptions ADD COLUMN cancelled_at TIMESTAMPTZ;\n"));
}

#[test]
fn sql_dialect_gaps_are_tolerated() {
    let text = "CREATE TABLE a (id INT);\n\nDO $$ BEGIN PERFORM weird_vendor_syntax(); END $$ ;;\n\nCREATE TABLE b (id INT);\n";
    let file = parsed("db/002.sql", text);
    let tables: Vec<&str> = file
        .symbols
        .iter()
        .filter(|s| s.kind == K::Table)
        .map(|s| s.name.as_str())
        .collect();
    assert!(tables.contains(&"a"), "{:#?}", summary(&file));
    let list = chunked(&file, text, &options(256));
    assert_covers(text, &list);
}

const PROTO: &str = r#"syntax = "proto3";

package billing.v1;

import "google/protobuf/timestamp.proto";

option go_package = "example.com/billing/v1";

// Subscription service.
service SubscriptionService {
  // Cancels a subscription.
  rpc CancelSubscription(CancelSubscriptionRequest) returns (CancelSubscriptionResponse);
  rpc Watch(WatchRequest) returns (stream Event) {}
}

// Cancel request.
message CancelSubscriptionRequest {
  string id = 1;
  message Reason {
    string text = 1;
  }
  Reason reason = 2;
}

message CancelSubscriptionResponse {}

enum Status {
  STATUS_UNSPECIFIED = 0;
  STATUS_ACTIVE = 1;
}
"#;

#[test]
fn protobuf() {
    let file = parsed("proto/billing/v1/billing.proto", PROTO);
    assert_eq!(
        (file.language, file.tier),
        (Language::Protobuf, Tier::Contract)
    );
    expect_symbols(
        &file,
        &[
            (K::Service, "SubscriptionService", 9, 14),
            (K::Rpc, "SubscriptionService.CancelSubscription", 11, 12),
            (K::Rpc, "SubscriptionService.Watch", 13, 13),
            (K::Message, "CancelSubscriptionRequest", 16, 23),
            (K::Field, "CancelSubscriptionRequest.id", 18, 18),
            (K::Message, "CancelSubscriptionRequest.Reason", 19, 21),
            (K::Field, "CancelSubscriptionRequest.Reason.text", 20, 20),
            (K::Field, "CancelSubscriptionRequest.reason", 22, 22),
            (K::Message, "CancelSubscriptionResponse", 25, 25),
            (K::Enum, "Status", 27, 30),
            (K::Field, "Status.STATUS_ACTIVE", 29, 29),
        ],
    );
    let rpc = symbol(&file, K::Rpc, "SubscriptionService.CancelSubscription");
    assert_eq!(
        rpc.signature,
        "rpc CancelSubscription(CancelSubscriptionRequest) returns (CancelSubscriptionResponse);"
    );
    assert_eq!(rpc.doc.as_deref(), Some("Cancels a subscription."));
    assert_eq!(
        symbol(&file, K::Service, "SubscriptionService").signature,
        "service SubscriptionService"
    );
    assert_eq!(imports(&file), ["google/protobuf/timestamp.proto"]);
    let skeleton = skeleton_of(&file, PROTO);
    assert!(skeleton.starts_with("// Subscription service.\nservice SubscriptionService {\n    // Cancels a subscription.\n    rpc CancelSubscription("), "{skeleton}");
    let list = chunked(&file, PROTO, &options(256));
    assert_covers(PROTO, &list);
    assert_eq!(
        chunk_for(&list, "SubscriptionService").kind,
        ChunkKind::Interface
    );
}

const OPENAPI: &str = r#"openapi: 3.0.3
info:
  title: Billing API
  version: 1.0.0
paths:
  /subscriptions/{id}:
    get:
      operationId: getSubscription
      summary: Get a subscription
      responses:
        '200':
          description: OK
    delete:
      operationId: cancelSubscription
      summary: Cancel a subscription
      responses:
        '204':
          description: Cancelled
  "/subscriptions":
    post:
      operationId: createSubscription
components:
  schemas:
    Subscription:
      type: object
      properties:
        id:
          type: string
"#;

#[test]
fn openapi_yaml() {
    let file = parsed("api/openapi.yaml", OPENAPI);
    assert_eq!(file.language, Language::Yaml);
    assert_eq!(file.dialect, Some(Dialect::OpenApi));
    expect_symbols(
        &file,
        &[
            (K::Key, "openapi", 1, 1),
            (K::Key, "info", 2, 4),
            (K::Endpoint, "GET /subscriptions/{id}", 7, 12),
            (K::Endpoint, "DELETE /subscriptions/{id}", 13, 18),
            (K::Endpoint, "POST /subscriptions", 20, 21),
            (K::Schema, "Subscription", 24, 28),
        ],
    );
    let get = symbol(&file, K::Endpoint, "GET /subscriptions/{id}");
    assert_eq!(
        get.signature,
        "GET /subscriptions/{id} (operationId: getSubscription)"
    );
    assert_eq!(get.doc.as_deref(), Some("Get a subscription"));
    assert_eq!(
        symbol(&file, K::Schema, "Subscription").signature,
        "schema Subscription: object"
    );
    let options = ChunkOptions {
        target_chars: 400,
        min_chars: 10,
        overlap_chars: 0,
    };
    let list = chunked(&file, OPENAPI, &options);
    assert_covers(OPENAPI, &list);
    let delete = chunk_for(&list, "DELETE /subscriptions/{id}");
    assert_eq!(delete.kind, ChunkKind::Endpoint);
    assert!(
        delete
            .text
            .starts_with("delete:\n      operationId: cancelSubscription")
    );
    let skeleton = skeleton_of(&file, OPENAPI);
    assert!(
        skeleton.contains(
            "GET /subscriptions/{id} (operationId: getSubscription) — Get a subscription\n"
        )
    );
}

#[test]
fn openapi_json() {
    let text = r#"{
  "openapi": "3.1.0",
  "info": {"title": "Billing", "version": "1"},
  "paths": {
    "/subscriptions/{id}": {
      "get": {"operationId": "getSubscription", "summary": "Get one"},
      "delete": {"operationId": "cancelSubscription"},
      "parameters": []
    }
  },
  "components": {"schemas": {"Subscription": {"type": "object"}}}
}
"#;
    let file = parsed("api/openapi.json", text);
    assert_eq!(
        (file.language, file.dialect),
        (Language::Json, Some(Dialect::OpenApi))
    );
    expect_symbols(
        &file,
        &[
            (K::Endpoint, "GET /subscriptions/{id}", 6, 6),
            (K::Endpoint, "DELETE /subscriptions/{id}", 7, 7),
            (K::Schema, "Subscription", 11, 11),
        ],
    );
    // `parameters` is not an HTTP method.
    assert_eq!(
        file.symbols
            .iter()
            .filter(|s| s.kind == K::Endpoint)
            .count(),
        2
    );
}

#[test]
fn asyncapi() {
    let v2 = "asyncapi: 2.6.0\nchannels:\n  user/signedup:\n    subscribe:\n      operationId: onUserSignedUp\n      summary: A user signed up.\n";
    let file = parsed("asyncapi.yaml", v2);
    assert_eq!(file.dialect, Some(Dialect::AsyncApi));
    expect_symbols(
        &file,
        &[
            (K::Channel, "user/signedup", 3, 6),
            (K::Endpoint, "SUBSCRIBE user/signedup", 4, 6),
        ],
    );
    let v3 = "asyncapi: 3.0.0\nchannels:\n  userSignedup:\n    address: user/signedup\noperations:\n  onUserSignedUp:\n    action: receive\n    channel:\n      $ref: '#/channels/userSignedup'\n";
    let file = parsed("asyncapi.yaml", v3);
    expect_symbols(
        &file,
        &[
            (K::Channel, "userSignedup", 3, 4),
            (K::Endpoint, "RECEIVE userSignedup", 6, 9),
        ],
    );
    assert_eq!(
        symbol(&file, K::Endpoint, "RECEIVE userSignedup").signature,
        "RECEIVE userSignedup (operationId: onUserSignedUp)"
    );
}

const MARKDOWN: &str = r#"# Billing

Intro text.

## Setup

Install it.

```sh
# not a heading
make
```

### Linux

Use apt.

Setext Heading
--------------

Done.
"#;

#[test]
fn markdown() {
    let file = parsed("docs/billing.md", MARKDOWN);
    assert_eq!(
        (file.language, file.tier),
        (Language::Markdown, Tier::Contract)
    );
    expect_symbols(
        &file,
        &[
            (K::Heading, "Billing", 1, 21),
            (K::Heading, "Billing > Setup", 5, 16),
            (K::Heading, "Billing > Setup > Linux", 14, 16),
            (K::Heading, "Billing > Setext Heading", 18, 21),
        ],
    );
    // The `#` line inside the fenced block is not a heading.
    assert_eq!(file.symbols.len(), 4);
    assert_eq!(
        skeleton_of(&file, MARKDOWN),
        "# Billing\n## Setup\n### Linux\n## Setext Heading\n"
    );
    let list = chunked(&file, MARKDOWN, &options(256));
    assert_covers(MARKDOWN, &list);
}

#[test]
fn markdown_large_sections_become_header_and_subsections() {
    let mut text = String::from("# Guide\n\nOverview paragraph.\n\n");
    for i in 0..6 {
        text.push_str(&format!("## Part {i}\n\n"));
        for j in 0..8 {
            text.push_str(&format!(
                "Paragraph {j} of part {i} explains one detail of the guide.\n\n"
            ));
        }
    }
    let file = parsed("docs/guide.md", &text);
    let list = chunked(&file, &text, &options(1000));
    assert_covers(&text, &list);
    let header = chunk_for(&list, "Guide");
    assert_eq!(header.kind, ChunkKind::Section);
    assert!(header.text.contains("Overview paragraph."));
    assert!(header.text.contains("## Part 0 …"), "{}", header.text);
    let part = chunk_for(&list, "Guide > Part 3");
    assert_eq!(part.parent, Some(header.ordinal));
    assert!(part.text.starts_with("## Part 3"));
}

#[test]
fn toml_manifest() {
    let text = "# Package manifest\n[package]\nname = \"demo\"\nversion = \"0.1.0\"\n\n[dependencies]\nserde = { version = \"1\", features = [\"derive\"] }\n\n[[bin]]\nname = \"demo\"\n\n[profile.release]\nlto = true\n";
    let file = parsed("Cargo.toml", text);
    assert_eq!(file.language, Language::Toml);
    expect_symbols(
        &file,
        &[
            (K::Section, "package", 2, 4),
            (K::Key, "package.name", 3, 3),
            (K::Section, "dependencies", 6, 7),
            (K::Key, "dependencies.serde", 7, 7),
            (K::Section, "bin", 9, 10),
            (K::Section, "profile.release", 12, 13),
            (K::Key, "profile.release.lto", 13, 13),
        ],
    );
    assert_eq!(symbol(&file, K::Section, "bin").signature, "[[bin]]");
    assert_eq!(
        symbol(&file, K::Key, "package.name").signature,
        "name = \"demo\""
    );
}

#[test]
fn plain_json_and_yaml_keys() {
    let text = "{\n  \"name\": \"web\",\n  \"scripts\": {\n    \"build\": \"vite build\",\n    \"test\": \"vitest\"\n  }\n}\n";
    let file = parsed("web/package.json", text);
    assert_eq!(file.dialect, None);
    expect_symbols(
        &file,
        &[
            (K::Key, "name", 2, 2),
            (K::Key, "scripts", 3, 6),
            (K::Key, "scripts.build", 4, 4),
            (K::Key, "scripts.test", 5, 5),
        ],
    );
    assert_eq!(
        symbol(&file, K::Key, "scripts.build").signature,
        "build: vite build"
    );

    let yaml = "# CI\nname: ci\non: [push]\njobs:\n  test:\n    runs-on: ubuntu-latest\n---\nsecond: document\n";
    let file = parsed(".github/workflows/ci.yml", yaml);
    expect_symbols(
        &file,
        &[
            (K::Key, "name", 2, 2),
            (K::Key, "jobs", 4, 6),
            (K::Key, "jobs.test", 5, 6),
            (K::Key, "second", 8, 8),
        ],
    );
}

#[test]
fn compose_and_kubernetes() {
    let compose = "services:\n  api:\n    image: example/api:1.0\n    ports:\n      - \"8080:8080\"\n  db:\n    image: postgres:16\nvolumes:\n  data: {}\n";
    let file = parsed("deploy/docker-compose.yml", compose);
    assert_eq!(file.dialect, Some(Dialect::Compose));
    expect_symbols(
        &file,
        &[
            (K::Key, "services", 1, 7),
            (K::Service, "services.api", 2, 5),
            (K::Service, "services.db", 6, 7),
            (K::Key, "volumes", 8, 9),
        ],
    );
    assert_eq!(
        symbol(&file, K::Service, "services.api").signature,
        "service api (image: example/api:1.0)"
    );

    let manifests = "apiVersion: apps/v1\nkind: Deployment\nmetadata:\n  name: web\n---\napiVersion: v1\nkind: Service\nmetadata:\n  name: web\n";
    let file = parsed("k8s/web.yaml", manifests);
    assert_eq!(file.dialect, Some(Dialect::Kubernetes));
    expect_symbols(
        &file,
        &[
            (K::Resource, "Deployment/web", 1, 4),
            (K::Resource, "Service/web", 6, 9),
        ],
    );
}

const DOCKERFILE: &str = r#"# syntax=docker/dockerfile:1
ARG RUST_VERSION=1.80

# Build stage.
FROM --platform=$BUILDPLATFORM rust:${RUST_VERSION} AS build
WORKDIR /app
RUN cargo build \
    --release

FROM gcr.io/distroless/cc
COPY --from=build /app/target/release/api /api
COPY --from=nginx:1.27 /etc/nginx/mime.types /etc/mime.types
EXPOSE 8080
ENTRYPOINT ["/api"]
"#;

#[test]
fn dockerfile() {
    let file = parsed("deploy/Dockerfile", DOCKERFILE);
    assert_eq!(
        (file.language, file.tier),
        (Language::Dockerfile, Tier::Contract)
    );
    expect_symbols(
        &file,
        &[
            (K::Stage, "build", 4, 8),
            (K::Stage, "gcr.io/distroless/cc", 10, 14),
        ],
    );
    let build = symbol(&file, K::Stage, "build");
    assert_eq!(build.doc.as_deref(), Some("Build stage."));
    assert_eq!(
        build.signature,
        "FROM --platform=$BUILDPLATFORM rust:${RUST_VERSION} AS build"
    );
    assert_eq!(build.name_line, 5);
    assert_eq!(
        imports(&file),
        ["rust:${RUST_VERSION}", "gcr.io/distroless/cc", "nginx:1.27"]
    );
    let list = chunked(&file, DOCKERFILE, &ChunkOptions::default());
    assert_covers(DOCKERFILE, &list);
    assert_eq!(
        skeleton_of(&file, DOCKERFILE),
        "FROM --platform=$BUILDPLATFORM rust:${RUST_VERSION} AS build — Build stage.\nFROM gcr.io/distroless/cc\n"
    );
}
