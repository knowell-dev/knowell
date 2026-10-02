//! Contract tests: the server runs in-process over a duplex pipe and is
//! driven by the official rmcp client against `FixtureTools`.

// Test code: panicking on unexpected values is the assertion mechanism.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic
)]

mod support;

use std::collections::BTreeMap;
use std::sync::Arc;

use knowell_mcp::{FILE_URI_TEMPLATE, FixtureTools, IMPACT_REVIEW, ONBOARD, ToolName};
use rmcp::ServiceExt;
use rmcp::model::{
    ErrorCode, GetPromptRequestParams, ProtocolVersion, ReadResourceRequestParams,
    ResourceContents, Tool,
};
use rmcp::service::{ClientLifecycleMode, ClientServiceExt, ServiceError};
use serde_json::{Value, json};
use support::{assert_matches_output_schema, call, connect, expect_error, open, structured, text};

fn tool_map(tools: Vec<Tool>) -> BTreeMap<String, Tool> {
    tools.into_iter().map(|t| (t.name.to_string(), t)).collect()
}

fn mcp_error_code(error: ServiceError) -> i32 {
    match error {
        ServiceError::McpError(data) => data.code.0,
        other => panic!("expected an MCP error, got {other:?}"),
    }
}

#[tokio::test]
async fn lists_fourteen_tools_with_valid_schemas_and_annotations() {
    let client = connect(Arc::new(FixtureTools::new())).await;
    let tools = client.list_all_tools().await.unwrap();
    let names: Vec<&str> = tools.iter().map(|t| t.name.as_ref()).collect();
    let expected: Vec<&str> = ToolName::ALL.iter().map(|t| t.as_str()).collect();
    assert_eq!(names, expected);
    assert_eq!(tools.len(), 14);

    for tool in &tools {
        let name = tool.name.as_ref();
        let kind = ToolName::parse(name).unwrap();
        let input = Value::Object(tool.input_schema.as_ref().clone());
        assert_eq!(input["type"], "object", "{name} input root");
        let problems = support::schema::check_well_formed(&input);
        assert!(problems.is_empty(), "{name} input schema: {problems:#?}");
        let output = Value::Object(
            tool.output_schema
                .as_ref()
                .expect("output schema")
                .as_ref()
                .clone(),
        );
        assert_eq!(output["type"], "object", "{name} output root");
        let problems = support::schema::check_well_formed(&output);
        assert!(problems.is_empty(), "{name} output schema: {problems:#?}");

        let description = tool.description.as_deref().unwrap_or_default();
        assert!(
            !description.is_empty() && description.len() <= 200,
            "{name}: {}",
            description.len()
        );
        assert!(tool.title.is_some(), "{name} title");

        let annotations = tool.annotations.as_ref().expect("annotations");
        let read_only = !matches!(name, "write_memory" | "save_checkpoint");
        assert_eq!(annotations.read_only_hint, Some(read_only), "{name}");
        assert_eq!(annotations.destructive_hint, Some(false), "{name}");
        assert_eq!(annotations.idempotent_hint, Some(read_only), "{name}");
        assert_eq!(annotations.open_world_hint, Some(false), "{name}");
        assert_eq!(kind.is_read_only(), read_only);

        // Every tool but open_workspace takes the explicit target.
        let props = input["properties"].as_object().unwrap();
        if name == "open_workspace" {
            assert!(props.contains_key("workspace") && props.contains_key("views"));
        } else {
            for key in ["context_id", "workspace", "views"] {
                assert!(props.contains_key(key), "{name} lacks {key}");
            }
        }
    }
    assert!(
        tool_map(tools.clone())["open_workspace"]
            .description
            .as_deref()
            .unwrap()
            .starts_with("Call first")
    );

    // Input schemas and descriptions are what clients put in the model's
    // context; output schemas are only used to validate results.
    let model_facing: usize = tools
        .iter()
        .map(|t| {
            t.name.len()
                + t.description.as_deref().map_or(0, str::len)
                + serde_json::to_string(t.input_schema.as_ref())
                    .unwrap()
                    .len()
        })
        .sum();
    let listing = serde_json::to_string(&tools).unwrap().len();
    eprintln!("tools/list: {listing} bytes in total, {model_facing} bytes model-facing");
    assert!(
        model_facing <= 14_000,
        "model-facing tool definitions grew to {model_facing} bytes (budget 14000)"
    );
    assert!(listing < 200_000, "tool listing grew to {listing} bytes");
}

#[tokio::test]
async fn server_info_instructions_and_protocol() {
    let client = connect(Arc::new(FixtureTools::new())).await;
    let info = client.peer_info().expect("server info");
    assert_eq!(
        info.server_info.as_ref().map(|s| s.name.as_str()),
        Some("knowell")
    );
    assert_eq!(info.protocol_version, ProtocolVersion::V_2025_11_25);
    let instructions = info.instructions.as_deref().unwrap();
    assert!(instructions.contains("Call open_workspace first"));
    assert!(instructions.contains("resume_task"));
    assert!(
        instructions.len() <= 1_500,
        "instructions: {}",
        instructions.len()
    );
    assert!(info.capabilities.tools.is_some());
    assert!(info.capabilities.prompts.is_some());
    assert!(info.capabilities.resources.is_some());
}

#[tokio::test]
async fn modern_lifecycle_2026_07_28_works() {
    let (server_io, client_io) = tokio::io::duplex(1 << 20);
    let server = knowell_mcp::KnowellServer::new(Arc::new(FixtureTools::new()));
    tokio::spawn(async move {
        if let Ok(running) = server.serve(server_io).await {
            let _ = running.waiting().await;
        }
    });
    let client = ()
        .serve_with_lifecycle(
            client_io,
            ClientLifecycleMode::Discover {
                preferred_versions: vec![ProtocolVersion::V_2026_07_28],
            },
        )
        .await
        .expect("discover handshake");
    assert_eq!(client.list_all_tools().await.unwrap().len(), 14);
    let context = open(&client, json!({})).await;
    let result = call(
        &client,
        "search",
        json!({"context_id": context, "query": "cancelSubscription"}),
    )
    .await;
    assert!(!structured(&result)["hits"].as_array().unwrap().is_empty());
}

#[tokio::test]
async fn every_tool_returns_schema_valid_structured_content() {
    let client = connect(Arc::new(FixtureTools::new())).await;
    let tools = tool_map(client.list_all_tools().await.unwrap());
    let run = |name: &'static str, args: Value| {
        let client = &client;
        let tools = &tools;
        async move {
            let result = call(client, name, args).await;
            assert_matches_output_schema(&tools[name], &result);
            assert!(!text(&result).trim().is_empty(), "{name} text rendering");
            structured(&result)
        }
    };

    let opened = run(
        "open_workspace",
        json!({"working_directory": "/home/dev/src/billing-api"}),
    )
    .await;
    let ctx = opened["context_id"].as_str().unwrap().to_owned();
    assert_eq!(opened["workspace"], "demo-shop");
    assert_eq!(opened["current_project"], "billing-api");
    assert_eq!(opened["manifest"].as_array().unwrap().len(), 3);
    assert_eq!(opened["rules"][0]["status"], "accepted");
    assert_eq!(opened["open_tasks"][0]["task_id"], "task-1");

    let search = run(
        "search",
        json!({"context_id": ctx, "query": "Where do we prevent the same payment being processed twice?"}),
    )
    .await;
    let hits = search["hits"].as_array().unwrap();
    assert!(!hits.is_empty());
    let first = &hits[0];
    for key in [
        "project",
        "view",
        "layer",
        "commit",
        "path",
        "lines",
        "content_hash",
        "freshness",
        "index_state",
        "why",
    ] {
        assert!(first["evidence"].get(key).is_some(), "evidence lacks {key}");
    }
    assert!(hits.iter().any(|h| {
        h["evidence"]["why"]
            .as_array()
            .unwrap()
            .iter()
            .any(|w| w["kind"] == "semantic")
    }));
    let id = first["id"].as_str().unwrap().to_owned();

    let fetched = run(
        "fetch",
        json!({"context_id": ctx, "ids": [id], "paths": [{"project": "storefront-web", "path": "src/lib/api/subscriptions.ts", "lines": {"start": 1, "end": 3}}]}),
    )
    .await;
    let items = fetched["items"].as_array().unwrap();
    assert_eq!(items.len(), 2);
    assert_eq!(items[0]["status"], "current");
    assert_eq!(items[0]["content"]["trust"], "untrusted");
    assert_eq!(items[1]["evidence"]["lines"], json!({"start": 1, "end": 3}));

    let inspected = run(
        "inspect_symbol",
        json!({"context_id": ctx, "symbol": "PaymentService.cancelSubscription"}),
    )
    .await;
    let symbol = &inspected["symbols"][0];
    assert_eq!(symbol["analysis"], "semantic");
    assert_eq!(symbol["references"][0]["relation"], "calls");
    assert_eq!(symbol["tests"][0]["relation"], "tests");

    let traced = run(
        "trace_flow",
        json!({"context_id": ctx, "symbol": "cancelSubscription", "project": "storefront-web"}),
    )
    .await;
    assert_eq!(traced["nodes"][0]["label"], "cancelSubscription");
    let edges = traced["edges"].as_array().unwrap();
    assert_eq!(edges.len(), 3, "depth 3 from the client: {edges:#?}");
    assert!(edges.iter().all(|e| e["resolution"] == "resolved"));
    assert_eq!(traced["truncated"], true);

    let impact = run(
        "analyze_impact",
        json!({"context_id": ctx, "change": {"kind": "symbol", "symbol": "PaymentService.cancelSubscription"}}),
    )
    .await;
    assert_eq!(impact["risk"]["level"], "medium");
    let impacted = impact["impacted"].as_array().unwrap();
    assert!(
        impacted
            .iter()
            .any(|i| i["evidence"]["project"] == "storefront-web")
    );
    assert!(
        impacted
            .iter()
            .all(|i| i["evidence"]["why"][0]["kind"] == "graph_path")
    );
    assert!(!impact["tests"].as_array().unwrap().is_empty());

    let file_impact = run(
        "analyze_impact",
        json!({"context_id": ctx, "change": {"kind": "file", "project": "billing-api", "path": "src/payments/payment.service.ts"}}),
    )
    .await;
    assert_eq!(file_impact["changed"].as_array().unwrap().len(), 2);

    let diff_impact = run(
        "analyze_impact",
        json!({"context_id": ctx, "change": {"kind": "diff", "project": "billing-api", "base": "tag:v2.1.0"}}),
    )
    .await;
    assert!(
        diff_impact["subject"]
            .as_str()
            .unwrap()
            .contains("tag:v2.1.0")
    );

    let patch = "--- a/src/payments/payment.service.ts\n+++ b/src/payments/payment.service.ts\n@@ -24,1 +24,1 @@\n-  async cancelSubscription(subscriptionId: string, reason?: string): Promise<void> {\n+  async cancelSubscription(subscriptionId: string): Promise<void> {\n";
    let pending = run(
        "analyze_impact",
        json!({"context_id": ctx, "change": {"kind": "patch", "project": "billing-api", "patch": patch}}),
    )
    .await;
    assert_eq!(pending["job"]["state"], "running");
    assert_eq!(pending["gaps"][0]["reason"], "job_pending");
    let job_id = pending["job"]["job_id"].as_str().unwrap().to_owned();
    let status = run(
        "index_status",
        json!({"context_id": ctx, "job_ids": [job_id]}),
    )
    .await;
    assert_eq!(status["jobs"][0]["kind"], "impact_analysis");
    let done = run(
        "analyze_impact",
        json!({"context_id": ctx, "job_id": job_id}),
    )
    .await;
    assert_eq!(done["job"]["state"], "succeeded");
    assert_eq!(
        done["changed"][0]["name"],
        "PaymentService.cancelSubscription"
    );

    let contracts = run("contracts", json!({"context_id": ctx})).await;
    let list = contracts["contracts"].as_array().unwrap();
    assert_eq!(list.len(), 3);
    assert!(
        list.iter()
            .any(|c| c["drift"][0]["code"] == "endpoint_without_client")
    );
    let only_drift = run("contracts", json!({"context_id": ctx, "only_drift": true})).await;
    assert_eq!(only_drift["contracts"].as_array().unwrap().len(), 1);

    let pack = run(
        "build_context",
        json!({"context_id": ctx, "task": "Add a cancellation reason to the cancel endpoint", "token_budget": 400}),
    )
    .await;
    let used = pack["budget"]["used"].as_u64().unwrap();
    assert!(used <= 400 && used > 0);
    assert!(
        pack["gaps"]
            .as_array()
            .unwrap()
            .iter()
            .any(|g| g["reason"] == "budget_exhausted")
    );
    let full = run(
        "build_context",
        json!({"context_id": ctx, "task": "cancel subscription"}),
    )
    .await;
    let sections: Vec<&str> = full["entries"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["section"].as_str().unwrap())
        .collect();
    for section in ["code", "tests", "contracts", "docs", "rules", "memory"] {
        assert!(
            sections.contains(&section),
            "missing {section}: {sections:?}"
        );
    }

    let history = run(
        "history",
        json!({"context_id": ctx, "project": "billing-api", "path": "src/payments/payment.service.ts"}),
    )
    .await;
    assert_eq!(history["commits"].as_array().unwrap().len(), 3);
    assert_eq!(history["commits"][0]["summary"]["origin"], "commit_message");
    assert_eq!(history["co_changed"].as_array().unwrap().len(), 2);
    assert_eq!(history["rationale"][0]["id"], "mem-2");

    let written = run(
        "write_memory",
        json!({
            "context_id": ctx,
            "scope": {"level": "project", "project": "billing-api"},
            "kind": "finding",
            "title": "Cancel publishes exactly one event",
            "body": "cancelSubscription publishes subscription.cancelled once per call.",
            "evidence": [symbol["id"]],
            "idempotency_key": "finding-1"
        }),
    )
    .await;
    assert_eq!(written["created"], true);
    assert_eq!(written["record"]["status"], "proposed");
    assert_eq!(written["record"]["evidence"][0]["project"], "billing-api");
    let again = run(
        "write_memory",
        json!({
            "context_id": ctx,
            "scope": {"level": "project", "project": "billing-api"},
            "kind": "finding",
            "title": "Cancel publishes exactly one event",
            "body": "cancelSubscription publishes subscription.cancelled once per call.",
            "idempotency_key": "finding-1"
        }),
    )
    .await;
    assert_eq!(again["created"], false);
    assert_eq!(again["record"]["id"], written["record"]["id"]);

    let memory = run(
        "read_memory",
        json!({"context_id": ctx, "project": "billing-api", "kinds": ["finding"]}),
    )
    .await;
    let records = memory["records"].as_array().unwrap();
    assert_eq!(
        records.len(),
        1,
        "task-scoped findings are excluded by `project`"
    );
    assert_eq!(records[0]["id"], written["record"]["id"]);

    let tasks = run("resume_task", json!({"context_id": ctx})).await;
    assert_eq!(tasks["tasks"][0]["task_id"], "task-1");
    let resumed = run(
        "resume_task",
        json!({"context_id": ctx, "task_id": "task-1"}),
    )
    .await;
    let task = &resumed["task"];
    assert_eq!(task["summary"]["status"], "in_progress");
    assert!(!task["changed_since"].as_array().unwrap().is_empty());
    assert_eq!(task["stale_knowledge"][0]["id"], "mem-3");
    assert_eq!(task["open_questions"][0]["trust"], "untrusted");

    let saved = run(
        "save_checkpoint",
        json!({
            "context_id": ctx,
            "goal": "Add a cancellation reason end to end",
            "progress": "Traced the cancel flow from the web client to the topic.",
            "decisions": [{"title": "Reason is optional", "body": "Older clients send no reason."}],
            "next_steps": ["Extend the spec"],
            "idempotency_key": "cp-a"
        }),
    )
    .await;
    assert_eq!(saved["created_task"], true);
    assert_eq!(saved["sequence"], 1);
    assert_eq!(saved["decisions"][0]["status"], "proposed");
    let new_task = saved["task_id"].as_str().unwrap().to_owned();
    let second = run(
        "save_checkpoint",
        json!({"context_id": ctx, "task_id": new_task, "progress": "Extended the spec.", "status": "done"}),
    )
    .await;
    assert_eq!(second["sequence"], 2);
    let retry = run(
        "save_checkpoint",
        json!({"context_id": ctx, "goal": "Add a cancellation reason end to end", "progress": "x", "idempotency_key": "cp-a"}),
    )
    .await;
    assert_eq!(retry["created"], false);
    assert_eq!(retry["task_id"], new_task.as_str());
    let resumed_new = run(
        "resume_task",
        json!({"context_id": ctx, "task_id": new_task}),
    )
    .await;
    assert_eq!(resumed_new["task"]["summary"]["status"], "done");
    assert_eq!(resumed_new["task"]["checkpoints"][0]["sequence"], 2);

    let status = run("index_status", json!({"context_id": ctx})).await;
    let projects = status["projects"].as_array().unwrap();
    assert_eq!(projects.len(), 3);
    let notifier = projects
        .iter()
        .find(|p| p["project"] == "notifier")
        .unwrap();
    assert_eq!(notifier["state"], "not_indexed");
    let storefront = projects
        .iter()
        .find(|p| p["project"] == "storefront-web")
        .unwrap();
    assert_ne!(
        storefront["latest_seen_commit"],
        storefront["indexed_commit"]
    );
    assert!(
        status["jobs"]
            .as_array()
            .unwrap()
            .iter()
            .any(|j| j["kind"] == "embed")
    );
}

#[tokio::test]
async fn result_ids_from_every_tool_are_fetchable() {
    let client = connect(Arc::new(FixtureTools::new())).await;
    let ctx = open(&client, json!({})).await;
    let mut ids: Vec<String> = Vec::new();
    let mut collect = |value: &Value, pointer: &str, key: &str| {
        for item in value
            .pointer(pointer)
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            if item.get("evidence").is_some_and(|e| !e.is_null())
                && let Some(id) = item.get(key).and_then(Value::as_str)
            {
                ids.push(id.to_owned());
            }
        }
    };
    let search = structured(
        &call(
            &client,
            "search",
            json!({"context_id": ctx, "query": "cancel subscriptions"}),
        )
        .await,
    );
    collect(&search, "/hits", "id");
    assert!(
        search["hits"]
            .as_array()
            .unwrap()
            .iter()
            .any(|h| h["kind"] == "contract")
    );
    let symbol = structured(
        &call(
            &client,
            "inspect_symbol",
            json!({"context_id": ctx, "symbol": "PaymentService.cancelSubscription"}),
        )
        .await,
    );
    collect(&symbol, "/symbols", "id");
    collect(&symbol, "/symbols/0/references", "id");
    collect(&symbol, "/symbols/0/tests", "id");
    let trace = structured(&call(&client, "trace_flow", json!({"context_id": ctx, "contract": "POST /v1/subscriptions/{id}/cancel", "direction": "both"})).await);
    collect(&trace, "/nodes", "id");
    let impact = structured(&call(&client, "analyze_impact", json!({"context_id": ctx, "change": {"kind": "symbol", "symbol": "PaymentService.processPayment"}})).await);
    collect(&impact, "/changed", "id");
    let pack = structured(
        &call(
            &client,
            "build_context",
            json!({"context_id": ctx, "task": "cancel"}),
        )
        .await,
    );
    collect(&pack, "/entries", "id");
    ids.sort();
    ids.dedup();
    assert!(ids.len() >= 8, "{ids:?}");
    for chunk in ids.chunks(20) {
        let fetched =
            structured(&call(&client, "fetch", json!({"context_id": ctx, "ids": chunk})).await);
        assert!(
            fetched.get("gaps").is_none(),
            "unfetchable ids: {fetched:#}"
        );
        assert_eq!(fetched["items"].as_array().unwrap().len(), chunk.len());
    }
    let contract_id = structured(
        &call(&client, "contracts", json!({"context_id": ctx, "limit": 1})).await,
    )["contracts"][0]["id"]
        .clone();
    let missing = structured(
        &call(
            &client,
            "fetch",
            json!({"context_id": ctx, "ids": [contract_id]}),
        )
        .await,
    );
    assert_eq!(
        missing["gaps"][0]["reason"], "not_found",
        "contract ids are not source ranges"
    );
}

#[tokio::test]
async fn workspace_target_without_context_and_view_pins() {
    let client = connect(Arc::new(FixtureTools::new())).await;
    let result = call(
        &client,
        "search",
        json!({"workspace": "demo-shop", "views": {"billing-api": "tag:v2.1.0"}, "query": "processPayment"}),
    )
    .await;
    let value = structured(&result);
    let hit = &value["hits"][0];
    assert_eq!(hit["evidence"]["view"], "tag:v2.1.0");
    assert_eq!(hit["evidence"]["why"][0]["kind"], "exact_symbol");
    assert_eq!(value["query_class"], "exact_symbol");

    let worktree = open(
        &client,
        json!({"working_directory": "/work/.worktree/retry/billing-api"}),
    )
    .await;
    let result = call(&client, "fetch", json!({"context_id": worktree, "paths": [{"project": "billing-api", "path": "docs/payments.md"}]})).await;
    let evidence = &structured(&result)["items"][0]["evidence"];
    assert_eq!(evidence["view"], "worktree");
    assert_eq!(evidence["layer"], "personal");
}

#[tokio::test]
async fn empty_and_partial_results_explain_why() {
    let client = connect(Arc::new(FixtureTools::new())).await;
    let ctx = open(&client, json!({"workspace": "demo-shop"})).await;
    let reasons = |value: &Value| -> Vec<String> {
        value["gaps"]
            .as_array()
            .map(|gaps| {
                gaps.iter()
                    .map(|g| g["reason"].as_str().unwrap().to_owned())
                    .collect()
            })
            .unwrap_or_default()
    };

    let nothing = structured(
        &call(
            &client,
            "search",
            json!({"context_id": ctx, "query": "zzqx quuxbar"}),
        )
        .await,
    );
    assert!(nothing.get("hits").is_none());
    let r = reasons(&nothing);
    assert!(
        r.contains(&"no_candidates_in_selected_ref".to_owned()),
        "{r:?}"
    );
    assert!(r.contains(&"project_not_indexed".to_owned()), "{r:?}");

    let notifier = structured(
        &call(
            &client,
            "search",
            json!({"context_id": ctx, "query": "email", "projects": ["notifier"]}),
        )
        .await,
    );
    assert_eq!(reasons(&notifier)[0], "project_not_indexed");

    let svelte = structured(
        &call(
            &client,
            "inspect_symbol",
            json!({"context_id": ctx, "symbol": "cancelSubscription", "project": "storefront-web"}),
        )
        .await,
    );
    assert!(reasons(&svelte).contains(&"no_reference_resolution_for_language".to_owned()));
    assert_eq!(svelte["symbols"][0]["references_complete"], false);
    assert_eq!(
        svelte["symbols"][0]["references"][0]["resolution"],
        "ambiguous"
    );

    let ambiguous = structured(
        &call(
            &client,
            "inspect_symbol",
            json!({"context_id": ctx, "symbol": "cancelSubscription"}),
        )
        .await,
    );
    assert_eq!(ambiguous["symbols"].as_array().unwrap().len(), 2);

    let secret = structured(
        &call(&client, "fetch", json!({"context_id": ctx, "paths": [{"project": "billing-api", "path": ".env.local"}, {"project": "billing-api", "path": "src/missing.ts"}]})).await,
    );
    assert_eq!(reasons(&secret), ["excluded_by_policy", "not_found"]);

    let missing_ref = structured(
        &call(
            &client,
            "open_workspace",
            json!({"views": {"storefront-web": "branch:does-not-exist"}}),
        )
        .await,
    );
    assert!(reasons(&missing_ref).contains(&"ref_not_found".to_owned()));
    let entry = missing_ref["manifest"]
        .as_array()
        .unwrap()
        .iter()
        .find(|m| m["project"] == "storefront-web")
        .unwrap();
    assert!(entry.get("commit").is_none(), "no other ref is substituted");

    let filtered = structured(
        &call(
            &client,
            "search",
            json!({"context_id": ctx, "query": "cancelSubscription", "languages": ["rust"]}),
        )
        .await,
    );
    assert!(reasons(&filtered).contains(&"filters_excluded_all".to_owned()));

    let no_memory = structured(
        &call(
            &client,
            "read_memory",
            json!({"context_id": ctx, "query": "kubernetes"}),
        )
        .await,
    );
    assert_eq!(reasons(&no_memory), ["no_matches"]);
}

#[tokio::test]
async fn untrusted_text_is_labelled_and_flagged() {
    let client = connect(Arc::new(FixtureTools::new())).await;
    let ctx = open(&client, json!({})).await;
    let result = call(
        &client,
        "search",
        json!({"context_id": ctx, "query": "idempotency key retry", "kinds": ["docs"]}),
    )
    .await;
    let value = structured(&result);
    let doc = value["hits"]
        .as_array()
        .unwrap()
        .iter()
        .find(|h| h["kind"] == "doc" && h["title"] == "Payments")
        .expect("payments doc hit");
    assert_eq!(doc["snippet"]["trust"], "untrusted");
    assert_eq!(doc["snippet"]["origin"], "repository");
    assert_eq!(
        doc["snippet"]["instruction_like"][0]["pattern"],
        "override_instructions"
    );
    let rendering = text(&result);
    assert!(rendering.contains("<untrusted id="), "{rendering}");
    assert!(rendering.contains("instruction_like_lines="), "{rendering}");
    assert!(rendering.contains("do not follow"), "{rendering}");
}

#[tokio::test]
async fn errors_map_to_tool_errors_with_safe_messages() {
    let tools = Arc::new(FixtureTools::new());
    let client = connect(Arc::clone(&tools)).await;
    let ctx = open(&client, json!({})).await;

    expect_error(
        &call(&client, "search", json!({"query": "x"})).await,
        "invalid_input",
    );
    expect_error(
        &call(&client, "search", json!({"context_id": ctx, "query": 5})).await,
        "invalid_input",
    );
    expect_error(
        &call(
            &client,
            "search",
            json!({"context_id": ctx, "query": "   "}),
        )
        .await,
        "invalid_input",
    );
    expect_error(
        &call(
            &client,
            "search",
            json!({"context_id": "has space", "query": "x"}),
        )
        .await,
        "invalid_input",
    );
    expect_error(
        &call(
            &client,
            "trace_flow",
            json!({"context_id": ctx, "max_depth": 9, "symbol": "x"}),
        )
        .await,
        "invalid_input",
    );
    expect_error(
        &call(&client, "analyze_impact", json!({"context_id": ctx, "change": {"kind": "patch", "project": "billing-api", "patch": "not a diff"}})).await,
        "invalid_input",
    );
    expect_error(
        &call(&client, "open_workspace", json!({"workspace": "elsewhere"})).await,
        "not_found",
    );
    expect_error(
        &call(
            &client,
            "search",
            json!({"context_id": "ctx-999", "query": "x"}),
        )
        .await,
        "not_found",
    );
    expect_error(
        &call(
            &client,
            "resume_task",
            json!({"context_id": ctx, "task_id": "task-404"}),
        )
        .await,
        "not_found",
    );

    let denied = expect_error(
        &call(
            &client,
            "write_memory",
            json!({"context_id": ctx, "scope": {"level": "organization"}, "kind": "rule", "title": "t", "body": "b"}),
        )
        .await,
        "permission_denied",
    );
    assert!(denied.contains("Do not retry"));

    let not_ready = expect_error(
        &call(
            &client,
            "history",
            json!({"context_id": ctx, "project": "notifier", "path": "main.go"}),
        )
        .await,
        "not_ready",
    );
    assert!(not_ready.contains("Retry after 5000 ms"), "{not_ready}");

    let internal = expect_error(
        &call(
            &client,
            "search",
            json!({"context_id": ctx, "query": "fixture:internal-error"}),
        )
        .await,
        "internal",
    );
    assert!(
        !internal.contains("10.0.0.5") && !internal.contains("5432"),
        "{internal}"
    );
    assert!(internal.contains("request"), "{internal}");

    let canary = "KNOWELL_CANARY_FAKE_0123456789";
    let secret = expect_error(
        &call(
            &client,
            "write_memory",
            json!({"context_id": ctx, "scope": {"level": "workspace"}, "kind": "note", "title": "creds", "body": format!("token is {canary}")}),
        )
        .await,
        "invalid_input",
    );
    assert!(!secret.contains(canary));

    let ctx_id = knowell_mcp::ContextId::new(ctx.clone()).unwrap();
    assert!(tools.expire_context(&ctx_id));
    let stale = expect_error(
        &call(&client, "search", json!({"context_id": ctx, "query": "x"})).await,
        "stale",
    );
    assert!(stale.contains("open_workspace"));

    let unknown = client
        .call_tool(rmcp::model::CallToolRequestParams::new("drop_tables"))
        .await
        .unwrap_err();
    assert_eq!(mcp_error_code(unknown), ErrorCode::INVALID_PARAMS.0);
}

#[tokio::test]
async fn concurrent_clients_keep_their_own_views() {
    let tools = Arc::new(FixtureTools::new());
    let a = Arc::new(connect(Arc::clone(&tools)).await);
    let b = Arc::new(connect(Arc::clone(&tools)).await);
    let ctx_a = open(&a, json!({"views": {"billing-api": "branch:main"}})).await;
    let ctx_b = open(&b, json!({"views": {"billing-api": "tag:v2.1.0"}})).await;
    // The same client may hold several contexts too.
    let ctx_a2 = open(&a, json!({"views": {"billing-api": "branch:development"}})).await;
    assert_ne!(ctx_a, ctx_b);
    assert_ne!(ctx_a, ctx_a2);

    let mut set = tokio::task::JoinSet::new();
    for round in 0..12 {
        for (client, ctx, view) in [
            (Arc::clone(&a), ctx_a.clone(), "branch:main"),
            (Arc::clone(&b), ctx_b.clone(), "tag:v2.1.0"),
            (Arc::clone(&a), ctx_a2.clone(), "branch:development"),
        ] {
            set.spawn(async move {
                let query = if round % 2 == 0 {
                    "cancelSubscription"
                } else {
                    "payment idempotency"
                };
                let result = call(
                    &client,
                    "search",
                    json!({"context_id": ctx, "query": query, "projects": ["billing-api"]}),
                )
                .await;
                let value = structured(&result);
                let hits = value["hits"].as_array().unwrap().clone();
                assert!(!hits.is_empty());
                for hit in hits {
                    assert_eq!(
                        hit["evidence"]["view"], view,
                        "context {ctx} saw another view"
                    );
                }
                (
                    ctx,
                    value["hits"][0]["evidence"]["commit"]
                        .as_str()
                        .unwrap()
                        .to_owned(),
                )
            });
        }
    }
    let mut commits: BTreeMap<String, std::collections::BTreeSet<String>> = BTreeMap::new();
    while let Some(joined) = set.join_next().await {
        let (ctx, commit) = joined.unwrap();
        commits.entry(ctx).or_default().insert(commit);
    }
    assert_eq!(commits.len(), 3);
    for (ctx, set) in &commits {
        assert_eq!(set.len(), 1, "context {ctx} saw several commits: {set:?}");
    }
    let distinct: std::collections::BTreeSet<_> = commits.values().flatten().collect();
    assert_eq!(distinct.len(), 3, "each context pins its own commit");
}

#[tokio::test]
async fn prompts_are_listed_and_rendered() {
    let client = connect(Arc::new(FixtureTools::new())).await;
    let prompts = client.list_all_prompts().await.unwrap();
    let names: Vec<&str> = prompts.iter().map(|p| p.name.as_str()).collect();
    assert_eq!(names, [ONBOARD, IMPACT_REVIEW]);
    let review = &prompts[1];
    let change = review
        .arguments
        .as_ref()
        .unwrap()
        .iter()
        .find(|a| a.name == "change")
        .unwrap();
    assert_eq!(change.required, Some(true));

    let mut args = serde_json::Map::new();
    args.insert("change".into(), json!("PaymentService.cancelSubscription"));
    let prompt = client
        .get_prompt(GetPromptRequestParams::new(IMPACT_REVIEW).with_arguments(args))
        .await
        .unwrap();
    let message = prompt.messages[0].content.as_text().unwrap().text.clone();
    assert!(
        message.contains("analyze_impact") && message.contains("PaymentService.cancelSubscription")
    );

    let onboard = client
        .get_prompt(GetPromptRequestParams::new(ONBOARD))
        .await
        .unwrap();
    assert!(
        onboard.messages[0]
            .content
            .as_text()
            .unwrap()
            .text
            .contains("open_workspace")
    );

    let missing = client
        .get_prompt(GetPromptRequestParams::new(IMPACT_REVIEW))
        .await
        .unwrap_err();
    assert_eq!(mcp_error_code(missing), ErrorCode::INVALID_PARAMS.0);
    let unknown = client
        .get_prompt(GetPromptRequestParams::new("nope"))
        .await
        .unwrap_err();
    assert_eq!(mcp_error_code(unknown), ErrorCode::RESOURCE_NOT_FOUND.0);
}

#[tokio::test]
async fn resource_template_is_listed_and_readable() {
    let client = connect(Arc::new(FixtureTools::new())).await;
    let templates = client.list_all_resource_templates().await.unwrap();
    assert_eq!(templates.len(), 1);
    assert_eq!(templates[0].uri_template, FILE_URI_TEMPLATE);
    assert_eq!(
        templates[0].uri_template,
        "knowell://{workspace}/{project}/{view}/{path}"
    );

    let uri = "knowell://demo-shop/billing-api/branch:main/docs/payments.md#L1-L5";
    let read = client
        .read_resource(ReadResourceRequestParams::new(uri))
        .await
        .unwrap();
    let ResourceContents::TextResourceContents {
        text,
        meta,
        uri: read_uri,
        ..
    } = &read.contents[0]
    else {
        panic!("expected text contents");
    };
    assert_eq!(read_uri, uri);
    assert!(text.starts_with("# Payments"));
    let meta = meta.as_ref().unwrap();
    assert_eq!(meta.0["knowell/trust"], "untrusted");
    assert_eq!(
        meta.0["knowell/evidence"]["lines"],
        json!({"start": 1, "end": 5})
    );
    assert_eq!(meta.0["knowell/instructionLike"][0]["line"], 5);

    for (bad, code) in [
        ("file:///etc/passwd", ErrorCode::INVALID_PARAMS.0),
        (
            "knowell://demo-shop/billing-api/branch:main/../secrets",
            ErrorCode::INVALID_PARAMS.0,
        ),
        (
            "knowell://demo-shop/billing-api/branch:main/src/nope.ts",
            ErrorCode::RESOURCE_NOT_FOUND.0,
        ),
        (
            "knowell://other/billing-api/branch:main/docs/payments.md",
            ErrorCode::RESOURCE_NOT_FOUND.0,
        ),
        (
            "knowell://demo-shop/notifier/branch:main/main.go",
            knowell_mcp::NOT_READY_CODE,
        ),
    ] {
        let error = client
            .read_resource(ReadResourceRequestParams::new(bad))
            .await
            .unwrap_err();
        assert_eq!(mcp_error_code(error), code, "{bad}");
    }
}

#[tokio::test]
async fn sample_inputs_match_input_schemas() {
    let client = connect(Arc::new(FixtureTools::new())).await;
    let tools = tool_map(client.list_all_tools().await.unwrap());
    let samples = [
        (
            "open_workspace",
            json!({"workspace": "demo-shop", "views": {"billing-api": "branch:main"}, "summary_budget_tokens": 1000}),
        ),
        (
            "search",
            json!({"context_id": "ctx-1", "query": "q", "kinds": ["code", "memory"], "limit": 5}),
        ),
        (
            "fetch",
            json!({"context_id": "ctx-1", "ids": ["kn:a:b:c#L1"], "paths": [{"project": "p", "path": "a/b.rs", "lines": {"start": 1, "end": 2}}]}),
        ),
        (
            "inspect_symbol",
            json!({"workspace": "demo-shop", "symbol": "A.b", "include": ["references"]}),
        ),
        (
            "trace_flow",
            json!({"context_id": "ctx-1", "contract": "topic:x", "direction": "both", "max_depth": 2}),
        ),
        (
            "analyze_impact",
            json!({"context_id": "ctx-1", "change": {"kind": "diff", "project": "p", "base": "tag:v1"}}),
        ),
        (
            "contracts",
            json!({"context_id": "ctx-1", "kinds": ["endpoint", "topic"], "only_drift": true}),
        ),
        (
            "build_context",
            json!({"context_id": "ctx-1", "task": "t", "token_budget": 1000, "include": ["code"]}),
        ),
        (
            "history",
            json!({"context_id": "ctx-1", "project": "p", "path": "a.rs", "include": ["blame"]}),
        ),
        (
            "read_memory",
            json!({"context_id": "ctx-1", "scopes": ["project"], "statuses": ["accepted"]}),
        ),
        (
            "write_memory",
            json!({"context_id": "ctx-1", "scope": {"level": "task", "task_id": "task-1"}, "kind": "note", "title": "t", "body": "b"}),
        ),
        (
            "resume_task",
            json!({"context_id": "ctx-1", "statuses": ["blocked"]}),
        ),
        (
            "save_checkpoint",
            json!({"context_id": "ctx-1", "task_id": "task-1", "progress": "p", "status": "blocked"}),
        ),
        (
            "index_status",
            json!({"context_id": "ctx-1", "projects": ["p"], "job_ids": ["job-1"]}),
        ),
    ];
    assert_eq!(samples.len(), 14);
    for (name, sample) in samples {
        let schema = Value::Object(tools[name].input_schema.as_ref().clone());
        let errors = support::schema::validate(&schema, &sample);
        assert!(errors.is_empty(), "{name}: {errors:#?}");
    }
    let bad = support::schema::validate(
        &Value::Object(tools["search"].input_schema.as_ref().clone()),
        &json!({"context_id": "ctx-1", "query": "", "limit": 0, "kinds": ["everything"]}),
    );
    assert_eq!(bad.len(), 3, "{bad:#?}");
}

#[tokio::test]
async fn views_must_be_a_project_to_ref_object() {
    let client = connect(Arc::new(FixtureTools::new())).await;
    for views in [
        json!([{"project": "billing-api", "view": "branch:main"}]),
        json!("branch:main"),
    ] {
        let result = call(&client, "open_workspace", json!({"views": views})).await;
        let message = expect_error(&result, "invalid_input");
        assert!(
            message.contains("object mapping project name to ref"),
            "{message}"
        );
    }
    let result = call(
        &client,
        "open_workspace",
        json!({"views": {"billing-api": "not a ref"}}),
    )
    .await;
    expect_error(&result, "invalid_input");
}
