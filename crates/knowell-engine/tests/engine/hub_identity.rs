//! Real middleware, MCP transport, engine and database authorization.

use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use knowell_auth::{Pepper, Principal, StoredToken, TokenScope, TokenScopes, issue_token};
use knowell_engine::StoreAccess;
use knowell_mcp::{HttpServerOptions, KnowellServer, OutputMode, streamable_http_router};
use knowell_server::{AppState, AuthenticatedCallers, ServerConfig, StoreTokenStore, build_router};
use knowell_store::identity::{self, GrantScope, NewGrant, NewPrincipal};
use knowell_store::{GrantRole, PrincipalId, PrincipalKind, hierarchy};
use serde_json::{Value, json};
use time::OffsetDateTime;
use tower::ServiceExt;

use crate::common::*;

async fn token(
    tokens: &StoreTokenStore,
    principal: Principal,
    scopes: TokenScopes,
) -> (String, StoredToken) {
    let pepper = Pepper::new(b"KNOWELL_CANARY_test_hub_pepper_01").unwrap();
    let now = OffsetDateTime::now_utc();
    let expires = principal
        .is_agent()
        .then_some(now + time::Duration::hours(1));
    let (plain, stored) = issue_token(&principal, scopes, expires, now, &pepper).unwrap();
    tokens
        .save_token(&stored, Some("synthetic HTTP test"), None)
        .await
        .unwrap();
    (plain.expose().to_owned(), stored)
}

async fn request(
    router: &axum::Router,
    token: &str,
    tool: &str,
    args: Value,
) -> (StatusCode, Value) {
    let req = Request::builder()
        .method("POST")
        .uri("/mcp")
        .header("host", "127.0.0.1:7420")
        .header("authorization", format!("Bearer {token}"))
        .header("content-type", "application/json")
        .header("accept", "application/json, text/event-stream")
        .header("mcp-protocol-version", "2025-11-25")
        .body(Body::from(
            json!({
                "jsonrpc": "2.0", "id": 1, "method": "tools/call",
                "params": {"name": tool, "arguments": args},
            })
            .to_string(),
        ))
        .unwrap();
    let reply = router.clone().oneshot(req).await.unwrap();
    let status = reply.status();
    let bytes = axum::body::to_bytes(reply.into_body(), 1 << 22)
        .await
        .unwrap();
    (status, serde_json::from_slice(&bytes).unwrap())
}

async fn call(router: &axum::Router, token: &str, tool: &str, args: Value) -> Value {
    let (status, reply) = request(router, token, tool, args).await;
    assert_eq!(status, StatusCode::OK, "{reply}");
    assert!(reply.get("error").is_none(), "{reply}");
    assert_ne!(reply["result"]["isError"], true, "{reply}");
    reply["result"]["structuredContent"].clone()
}

/// Every nested evidence record must stay within the granted project.
fn only_project(value: &Value, project: &str) {
    match value {
        Value::Object(map) => {
            if let Some(Value::String(p)) = map.get("project") {
                assert_eq!(p, project, "unauthorized project in response");
            }
            for nested in map.values() {
                only_project(nested, project);
            }
        }
        Value::Array(items) => {
            for nested in items {
                only_project(nested, project);
            }
        }
        _ => {}
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn hub_http_obeys_current_grants_scopes_and_revocations() {
    if !git_available() {
        return;
    }
    let db = require_db!();
    let ws = fixture_workspace();
    let data = tempfile::tempdir().unwrap();
    let engine = indexed_engine_with_access(
        &db,
        &ws,
        data.path(),
        Arc::new(StoreAccess::new(db.store.clone(), name("acme"))),
    )
    .await;
    let mut conn = db.store.acquire().await.unwrap();
    let org = hierarchy::find_organization(&mut conn, &name("acme"))
        .await
        .unwrap()
        .unwrap();
    let workspace = hierarchy::find_workspace(&mut conn, org.id, &name("acme-goods"))
        .await
        .unwrap()
        .unwrap();
    let api = hierarchy::find_project(&mut conn, workspace.id, &name("billing-api"))
        .await
        .unwrap()
        .unwrap();
    let web = hierarchy::find_project(&mut conn, workspace.id, &name("storefront-web"))
        .await
        .unwrap()
        .unwrap();
    for (user, label) in [(alice(), "alice"), (bob(), "bob")] {
        identity::create_principal(
            &mut conn,
            org.id,
            &NewPrincipal {
                id: Some(PrincipalId(user.as_uuid())),
                kind: PrincipalKind::User,
                name: name(label),
                display_name: None,
            },
        )
        .await
        .unwrap();
    }
    identity::create_grant(
        &mut conn,
        &NewGrant {
            principal: PrincipalId(alice().as_uuid()),
            role: GrantRole::Admin,
            scope: GrantScope::Organization,
            created_by: None,
        },
    )
    .await
    .unwrap();
    let grant = identity::create_grant(
        &mut conn,
        &NewGrant {
            principal: PrincipalId(bob().as_uuid()),
            role: GrantRole::Member,
            scope: GrantScope::Project(api.id),
            created_by: None,
        },
    )
    .await
    .unwrap();
    let service_id = uuid::Uuid::from_u128(0xC1);
    identity::create_principal(
        &mut conn,
        org.id,
        &NewPrincipal {
            id: Some(PrincipalId(service_id)),
            kind: PrincipalKind::ServiceAccount,
            name: name("ci-reader"),
            display_name: None,
        },
    )
    .await
    .unwrap();
    identity::create_grant(
        &mut conn,
        &NewGrant {
            principal: PrincipalId(service_id),
            role: GrantRole::Viewer,
            scope: GrantScope::Project(api.id),
            created_by: None,
        },
    )
    .await
    .unwrap();
    drop(conn);

    let tokens = StoreTokenStore::new(db.store.clone(), name("acme"));
    let (alice_token, _) = token(
        &tokens,
        Principal::User(alice()),
        TokenScopes::new([TokenScope::Read, TokenScope::Write, TokenScope::Admin]).unwrap(),
    )
    .await;
    let (bob_read, bob_stored) =
        token(&tokens, Principal::User(bob()), TokenScopes::read_only()).await;
    let (bob_write, _) = token(
        &tokens,
        Principal::User(bob()),
        TokenScopes::new([TokenScope::Read, TokenScope::Write]).unwrap(),
    )
    .await;
    let (service_token, _) = token(
        &tokens,
        Principal::ServiceAccount(knowell_auth::ServiceAccountId::new(service_id)),
        TokenScopes::read_only(),
    )
    .await;
    let (agent_token, _) = token(
        &tokens,
        Principal::Agent {
            on_behalf_of: bob(),
            client: name("verified-agent"),
            session: knowell_auth::AgentSessionId::new(uuid::Uuid::from_u128(0xA6E17)),
        },
        TokenScopes::read_only(),
    )
    .await;
    // Inspect nested typed evidence for authorization independently of the
    // default Source presentation, which intentionally omits structuredContent.
    let mcp = KnowellServer::new(Arc::new(engine))
        .with_caller_resolver(Arc::new(AuthenticatedCallers))
        .with_output_mode(OutputMode::Full);
    let options = HttpServerOptions {
        stateful_sessions: false,
        json_response: true,
        ..HttpServerOptions::default()
    };
    let config = ServerConfig::new(
        knowell_config::ServerRole::Hub,
        "127.0.0.1:7420".parse().unwrap(),
        name("acme"),
    );
    let state = AppState::builder(config)
        .with_store(db.store.clone())
        .with_store_tokens()
        .with_pepper(Pepper::new(b"KNOWELL_CANARY_test_hub_pepper_01").unwrap())
        .with_mcp(streamable_http_router(mcp, &options))
        .build()
        .unwrap();
    let router = build_router(state);

    for credential in [&service_token, &agent_token] {
        let opened = call(
            &router,
            credential,
            "open_workspace",
            json!({"workspace": "acme-goods"}),
        )
        .await;
        assert_eq!(opened["manifest"].as_array().unwrap().len(), 1);
        only_project(&opened, "billing-api");
    }

    let all = call(
        &router,
        &alice_token,
        "open_workspace",
        json!({"workspace": "acme-goods"}),
    )
    .await;
    assert_eq!(all["manifest"].as_array().unwrap().len(), 10);
    let opened = call(
        &router,
        &bob_read,
        "open_workspace",
        json!({"workspace": "acme-goods"}),
    )
    .await;
    assert_eq!(opened["manifest"].as_array().unwrap().len(), 1);
    only_project(&opened, "billing-api");
    let context = opened["context_id"].clone();
    let found = call(
        &router,
        &bob_read,
        "search",
        json!({"context_id": context, "query": "cancel subscription", "limit": 30}),
    )
    .await;
    assert!(!found["hits"].as_array().unwrap().is_empty());
    only_project(&found, "billing-api");
    let graph = call(
        &router,
        &bob_read,
        "trace_flow",
        json!({"context_id": context, "symbol": "SubscriptionService", "project": "billing-api"}),
    )
    .await;
    assert!(!graph["nodes"].as_array().unwrap().is_empty());
    only_project(&graph, "billing-api");
    let pack = call(
        &router,
        &bob_read,
        "build_context",
        json!({"context_id": context, "task": "cancel a subscription", "token_budget": 4000}),
    )
    .await;
    only_project(&pack, "billing-api");

    let note = json!({"context_id": context, "scope": {"level": "project", "project": "billing-api"},
        "kind": "note", "title": "billing decision", "body": "Synthetic billing note."});
    let (_, denied) = request(&router, &bob_read, "write_memory", note.clone()).await;
    assert_eq!(denied["result"]["isError"], true, "{denied}");
    assert!(denied.to_string().contains("permission_denied"));
    let written = call(&router, &bob_write, "write_memory", note).await;
    assert!(written.to_string().contains("proposed"), "{written}");
    call(
        &router,
        &alice_token,
        "write_memory",
        json!({"workspace": "acme-goods",
        "scope": {"level": "project", "project": "storefront-web"}, "kind": "note",
        "title": "private web decision", "body": "Synthetic private web note."}),
    )
    .await;
    let memory = call(
        &router,
        &bob_read,
        "read_memory",
        json!({"context_id": context}),
    )
    .await;
    assert!(memory.to_string().contains("billing decision"), "{memory}");
    assert!(!memory.to_string().contains("private web"));
    only_project(&memory, "billing-api");
    let (_, stolen) = request(
        &router,
        &bob_read,
        "search",
        json!({"context_id": all["context_id"], "query": "subscription"}),
    )
    .await;
    assert_eq!(stolen["result"]["isError"], true, "{stolen}");

    // Keep MCP enabled through another grant so old contexts must be
    // re-filtered by the engine, not merely rejected by the middleware.
    let mut conn = db.store.acquire().await.unwrap();
    identity::create_grant(
        &mut conn,
        &NewGrant {
            principal: PrincipalId(bob().as_uuid()),
            role: GrantRole::Member,
            scope: GrantScope::Project(web.id),
            created_by: None,
        },
    )
    .await
    .unwrap();
    identity::delete_grant(&mut conn, grant.id).await.unwrap();
    drop(conn);
    let filtered = call(
        &router,
        &bob_read,
        "search",
        json!({"context_id": context, "query": "cancel subscription"}),
    )
    .await;
    let filtered: knowell_mcp::tools::SearchOutput = serde_json::from_value(filtered).unwrap();
    assert!(filtered.hits.is_empty(), "{filtered:?}");
    let graph = call(
        &router,
        &bob_read,
        "trace_flow",
        json!({"context_id": context, "symbol": "SubscriptionService", "project": "billing-api"}),
    )
    .await;
    let graph: knowell_mcp::tools::TraceFlowOutput = serde_json::from_value(graph).unwrap();
    assert!(graph.nodes.is_empty(), "{graph:?}");
    let pack = call(
        &router,
        &bob_read,
        "build_context",
        json!({"context_id": context, "task": "cancel a subscription", "token_budget": 4000}),
    )
    .await;
    assert!(!pack.to_string().contains("billing decision"));
    only_project(&pack, "storefront-web");
    let memory = call(
        &router,
        &bob_read,
        "read_memory",
        json!({"context_id": context}),
    )
    .await;
    assert!(!memory.to_string().contains("billing decision"));
    let updated = call(
        &router,
        &bob_read,
        "open_workspace",
        json!({"workspace": "acme-goods"}),
    )
    .await;
    assert_eq!(updated["manifest"].as_array().unwrap().len(), 1);
    only_project(&updated, "storefront-web");
    tokens
        .revoke_token(bob_stored.id, OffsetDateTime::now_utc())
        .await
        .unwrap();
    assert_eq!(
        request(&router, &bob_read, "open_workspace", json!({}))
            .await
            .0,
        StatusCode::UNAUTHORIZED
    );
    let mut conn = db.store.acquire().await.unwrap();
    identity::set_principal_disabled(
        &mut conn,
        PrincipalId(bob().as_uuid()),
        Some(OffsetDateTime::now_utc()),
    )
    .await
    .unwrap();
    drop(conn);
    assert_eq!(
        request(&router, &bob_write, "open_workspace", json!({}))
            .await
            .0,
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        request(
            &router,
            "KNOWELL_CANARY_invalid_token",
            "open_workspace",
            json!({})
        )
        .await
        .0,
        StatusCode::UNAUTHORIZED
    );
    db.store.close().await;
}
