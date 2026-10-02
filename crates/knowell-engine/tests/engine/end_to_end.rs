//! The whole engine on the acme-goods fixture: index, then every tool.

use std::collections::BTreeSet;
use std::sync::Arc;

use knowell_auth::GrantSet;
use knowell_engine::{Access, HybridRetriever};
use knowell_eval::{Bm25Retriever, GrepRetriever, QuerySet, Retriever, run, walk_fixture};
use knowell_index::Priority;
use knowell_mcp::tools::{
    AnalyzeImpactInput, BuildContextInput, ChangeSubject, ContractsInput, FetchInput, HistoryInput,
    IndexStatusInput, InspectSymbolInput, MemoryKind, MemoryScope, MemoryStatus,
    OpenWorkspaceInput, ReadMemoryInput, ResumeTaskInput, SaveCheckpointInput, ScopeLevel,
    SearchInput, TierState, TraceFlowInput, WriteMemoryInput,
};
use knowell_mcp::{
    FileLocator, FreshnessTier, GapReason, KnowellTools, MatchReason, SymbolRef, Target, ViewLayer,
};
use knowell_server::{EngineContext, EngineRequest, MemoryAction, MemoryDecision};

use crate::common::{
    alice, alice_caller, bob_caller, fixture_workspace, git_available, indexed_engine, name,
    require_db,
};

fn context_target(id: &knowell_mcp::ContextId) -> Target {
    Target::context(id.clone())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn engine_serves_every_tool_over_the_indexed_fixture() {
    if !git_available() {
        eprintln!("skipping: git is not installed");
        return;
    }
    let db = require_db!();
    let mut ws = fixture_workspace();
    // One local-only project names a cloud provider: nothing may be sent.
    for project in &mut ws.resolved.projects {
        if project.name.as_str() == "handbook"
            && let Some(provider) = project.embedding.provider.as_mut()
        {
            provider.value = name("cloud");
        }
    }
    let data = tempfile::tempdir().unwrap();
    let engine = indexed_engine(&db, &ws, data.path()).await;
    let agent_a = alice_caller();
    let agent_b = bob_caller();

    // ---------------------------------------------------------- open_workspace
    let opened = engine
        .open_workspace(&agent_a, OpenWorkspaceInput::default())
        .await
        .unwrap();
    assert_eq!(opened.workspace.as_str(), "acme-goods");
    assert_eq!(opened.manifest.len(), 10, "{:?}", opened.manifest);
    for view in &opened.manifest {
        assert!(view.commit.is_some(), "{view:?}");
        assert_eq!(view.layer, ViewLayer::Shared);
    }
    assert_eq!(opened.projects.len(), 10);
    let billing = opened
        .projects
        .iter()
        .find(|p| p.name.as_str() == "billing-api")
        .unwrap();
    assert!(
        billing.languages.contains(&"typescript".to_owned()),
        "{billing:?}"
    );
    let ctx = context_target(&opened.context_id);

    // ------------------------------------------------------------------ search
    let found = engine
        .search(
            &agent_a,
            SearchInput {
                target: ctx.clone(),
                query: "cancel subscription".into(),
                limit: Some(40),
                ..SearchInput::default()
            },
        )
        .await
        .unwrap();
    let projects: BTreeSet<&str> = found
        .hits
        .iter()
        .map(|h| h.evidence.project.as_str())
        .collect();
    eprintln!(
        "search hits: {:#?}",
        found
            .hits
            .iter()
            .map(|h| (
                h.evidence.project.as_str(),
                h.evidence.path.as_str(),
                h.title.as_str()
            ))
            .collect::<Vec<_>>()
    );
    for expected in ["billing-api", "storefront-web", "mobile-app"] {
        assert!(
            projects.contains(expected),
            "{expected} missing from {projects:?}"
        );
    }
    for hit in &found.hits {
        let e = &hit.evidence;
        assert_eq!(e.commit.as_str().len(), 40);
        assert!(e.lines.start() >= 1 && e.lines.end() >= e.lines.start());
        assert!(!e.why.is_empty(), "{hit:?}");
        assert!(hit.id.as_str().starts_with("kn:"));
        assert!(hit.snippet.is_some());
    }
    assert!(found.hits.iter().any(|h| {
        h.evidence
            .why
            .iter()
            .any(|w| matches!(w, MatchReason::Lexical { .. }))
    }));
    assert!(
        found
            .hits
            .iter()
            .any(|h| h.evidence.freshness == FreshnessTier::T2Embeddings),
        "the semantic source should contribute"
    );
    assert!(
        found
            .gaps
            .iter()
            .any(|g| g.reason == GapReason::EmbeddingsNotReady
                && g.message.contains("handbook")
                && g.message.contains("nothing was sent")),
        "{:?}",
        found.gaps
    );

    // ------------------------------------------------------------------- fetch
    let first = found.hits.first().unwrap();
    let fetched = engine
        .fetch(
            &agent_a,
            FetchInput {
                target: ctx.clone(),
                ids: vec![first.id.clone()],
                paths: vec![FileLocator {
                    project: name("billing-api"),
                    path: knowell_core::RepoPath::new("src/subscriptions/subscription.service.ts")
                        .unwrap(),
                    lines: None,
                }],
                context_lines: None,
            },
        )
        .await
        .unwrap();
    assert_eq!(fetched.items.len(), 2, "{:?}", fetched.gaps);
    assert!(
        fetched.items[1]
            .content
            .text()
            .contains("cancelSubscription")
    );
    assert_eq!(
        fetched.items[0].evidence.content_hash,
        first.evidence.content_hash
    );

    // ---------------------------------------------------------- inspect_symbol
    let inspected = engine
        .inspect_symbol(
            &agent_a,
            InspectSymbolInput {
                target: ctx.clone(),
                symbol: SymbolRef {
                    id: None,
                    symbol: Some("SubscriptionService.cancelSubscription".into()),
                    project: None,
                },
                include: Vec::new(),
                limit: None,
            },
        )
        .await
        .unwrap();
    assert_eq!(inspected.symbols.len(), 1, "{inspected:?}");
    let symbol = &inspected.symbols[0];
    assert_eq!(
        symbol.qualified_name,
        "SubscriptionService.cancelSubscription"
    );
    assert_eq!(symbol.definition.project.as_str(), "billing-api");
    assert_eq!(
        symbol.definition.path.as_str(),
        "src/subscriptions/subscription.service.ts"
    );
    assert!(
        symbol
            .signature
            .as_ref()
            .unwrap()
            .text()
            .contains("cancelSubscription")
    );
    assert!(
        symbol
            .doc
            .as_ref()
            .is_some_and(|d| d.text().contains("ADR-0005"))
    );
    assert!(
        !symbol.references.is_empty(),
        "importers of the service file"
    );
    assert!(
        inspected
            .gaps
            .iter()
            .any(|g| g.reason == GapReason::NoReferenceResolutionForLanguage)
    );

    // ----------------------------------------------------------- build_context
    let pack = engine
        .build_context(
            &agent_a,
            BuildContextInput {
                target: ctx.clone(),
                task: Some("let members cancel their subscription at period end".into()),
                token_budget: Some(1500),
                ..BuildContextInput::default()
            },
        )
        .await
        .unwrap();
    assert!(!pack.entries.is_empty());
    assert!(
        pack.budget.used <= pack.budget.requested,
        "{:?}",
        pack.budget
    );
    let total: u32 = pack.entries.iter().map(|e| e.estimated_tokens).sum();
    assert!(total <= 1500, "{total}");
    for entry in &pack.entries {
        let evidence = entry
            .evidence
            .as_ref()
            .expect("code entries cite their source");
        assert_eq!(evidence.commit.as_str().len(), 40);
        assert!(!entry.why_relevant.is_empty());
    }

    // ------------------------------------------------------------ index_status
    let status = engine
        .index_status(
            &agent_a,
            IndexStatusInput {
                target: ctx.clone(),
                ..IndexStatusInput::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(status.projects.len(), 10);
    let billing_status = status
        .projects
        .iter()
        .find(|p| p.project.as_str() == "billing-api")
        .unwrap();
    assert_eq!(billing_status.tiers.len(), 4);
    assert!(
        billing_status
            .tiers
            .iter()
            .take(3)
            .all(|t| t.state == TierState::Ready),
        "{:?}",
        billing_status.tiers
    );
    assert_eq!(
        billing_status.indexed_commit,
        billing_status.latest_seen_commit
    );
    let handbook_status = status
        .projects
        .iter()
        .find(|p| p.project.as_str() == "handbook")
        .unwrap();
    assert_eq!(handbook_status.tiers[2].state, TierState::Unavailable);

    // --------------------------------------------------- trace / impact / etc.
    let trace = engine
        .trace_flow(
            &agent_a,
            TraceFlowInput {
                target: ctx.clone(),
                symbol: Some("SubscriptionService".into()),
                project: Some(name("billing-api")),
                direction: Some(knowell_mcp::tools::FlowDirection::Upstream),
                ..TraceFlowInput::default()
            },
        )
        .await
        .unwrap();
    assert!(!trace.nodes.is_empty());
    // knowell-link runs as the indexer's T3 relation stage, so relations are
    // ready and the trace must not claim otherwise.
    assert!(
        !trace
            .gaps
            .iter()
            .any(|g| g.reason == GapReason::RelationsNotReady),
        "{:?}",
        trace.gaps
    );

    let patch = "--- a/src/subscriptions/subscription.service.ts\n+++ b/src/subscriptions/subscription.service.ts\n@@ -1,1 +1,2 @@\n+// reviewed\n";
    let base_first_line = fetched.items[1]
        .content
        .text()
        .lines()
        .next()
        .unwrap()
        .to_owned();
    let patch = format!("{patch} {base_first_line}\n");
    let impact = engine
        .analyze_impact(
            &agent_a,
            AnalyzeImpactInput {
                target: ctx.clone(),
                change: Some(ChangeSubject::Patch {
                    project: name("billing-api"),
                    patch,
                }),
                ..AnalyzeImpactInput::default()
            },
        )
        .await
        .unwrap();
    assert!(!impact.changed.is_empty(), "{impact:?}");
    assert!(impact.risk.is_some());
    let symbol_impact = engine
        .analyze_impact(
            &agent_a,
            AnalyzeImpactInput {
                target: ctx.clone(),
                change: Some(ChangeSubject::Symbol {
                    symbol: SymbolRef {
                        id: None,
                        symbol: Some("SubscriptionService.cancelSubscription".into()),
                        project: Some(name("billing-api")),
                    },
                }),
                ..AnalyzeImpactInput::default()
            },
        )
        .await
        .unwrap();
    assert!(
        !symbol_impact.impacted.is_empty(),
        "importers are affected: {symbol_impact:?}"
    );
    assert!(
        symbol_impact
            .impacted
            .iter()
            .any(|i| i.evidence.path.as_str() == "src/subscriptions/subscriptions.controller.ts"),
        "{symbol_impact:?}"
    );
    assert!(
        trace
            .nodes
            .iter()
            .any(|n| n.label.contains("subscriptions.controller")
                || n.label.starts_with("SubscriptionsController")),
        "{trace:?}"
    );

    let contracts = engine
        .contracts(
            &agent_a,
            ContractsInput {
                target: ctx.clone(),
                query: Some("POST /v1/subscriptions/{id}/cancel".into()),
                ..ContractsInput::default()
            },
        )
        .await
        .unwrap();
    // Contracts are extracted by the T3 relation stage.
    assert!(
        !contracts
            .gaps
            .iter()
            .any(|g| g.message.starts_with("contracts_not_extracted")),
        "{:?}",
        contracts.gaps
    );
    assert!(!contracts.contracts.is_empty(), "{contracts:?}");

    let history = engine
        .history(
            &agent_a,
            HistoryInput {
                target: ctx.clone(),
                project: Some(name("billing-api")),
                path: Some(
                    knowell_core::RepoPath::new("src/subscriptions/subscription.service.ts")
                        .unwrap(),
                ),
                ..HistoryInput::default()
            },
        )
        .await
        .unwrap();
    assert!(!history.gaps.is_empty());

    // ------------------------------------------------------------ write_memory
    let written = engine
        .write_memory(
            &agent_a,
            WriteMemoryInput {
                target: ctx.clone(),
                scope: MemoryScope {
                    level: ScopeLevel::Project,
                    project: Some(name("billing-api")),
                    task_id: None,
                },
                kind: MemoryKind::Decision,
                title: "Cancellation keeps benefits until the period ends".into(),
                body: "cancelSubscription marks the row cancelled but keeps benefits (ADR-0005).".into(),
                related_symbols: vec!["src/subscriptions/subscription.service.ts#SubscriptionService.cancelSubscription".into()],
                evidence: vec![symbol.id.clone()],
                supersedes: None,
                idempotency_key: Some("decision-1".into()),
            },
        )
        .await
        .unwrap();
    assert!(written.created);
    assert_eq!(
        written.record.status,
        MemoryStatus::Proposed,
        "agents only propose"
    );
    assert_eq!(written.record.evidence.len(), 1);
    let again = engine
        .write_memory(
            &agent_a,
            WriteMemoryInput {
                target: ctx.clone(),
                scope: MemoryScope {
                    level: ScopeLevel::Project,
                    project: Some(name("billing-api")),
                    task_id: None,
                },
                kind: MemoryKind::Decision,
                title: "Cancellation keeps benefits until the period ends".into(),
                body: "same".into(),
                related_symbols: Vec::new(),
                evidence: Vec::new(),
                supersedes: None,
                idempotency_key: Some("decision-1".into()),
            },
        )
        .await
        .unwrap();
    assert!(!again.created);
    assert_eq!(again.record.id, written.record.id);
    let token = format!("ghp_{}", "FAKE".repeat(9));
    let secret = engine
        .write_memory(
            &agent_a,
            WriteMemoryInput {
                target: ctx.clone(),
                scope: MemoryScope {
                    level: ScopeLevel::Workspace,
                    project: None,
                    task_id: None,
                },
                kind: MemoryKind::Note,
                title: "deploy token".into(),
                body: format!("use {token} for deploys"),
                related_symbols: Vec::new(),
                evidence: Vec::new(),
                supersedes: None,
                idempotency_key: None,
            },
        )
        .await
        .unwrap_err();
    assert_eq!(secret.kind(), "invalid_input");
    let shown = format!("{secret} {}", secret.client_message("r"));
    assert!(
        !shown.contains(&token) && !shown.contains("FAKEFAKE"),
        "{shown}"
    );
    let memory = engine
        .read_memory(
            &agent_a,
            ReadMemoryInput {
                target: ctx.clone(),
                ..ReadMemoryInput::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(memory.records.len(), 1);
    assert_eq!(memory.records[0].status, MemoryStatus::Proposed);

    // ------------------------------------------------- REST: accept by a human
    let rest_context = EngineContext {
        principal: knowell_auth::Principal::User(alice()),
        scopes: None,
        grants: Arc::new({
            let mut g = GrantSet::new();
            g.add(
                knowell_auth::Grant::new(
                    knowell_auth::Principal::User(alice()),
                    knowell_auth::Role::Admin,
                    knowell_auth::ResourceScope::Organization,
                )
                .unwrap(),
            );
            g
        }),
        visible: knowell_auth::ProjectFilter::default(),
        request_id: knowell_auth::RequestId::new("req-1").unwrap(),
        audit: Arc::new(knowell_server::MemoryAuditSink::new()),
    };
    let listed = knowell_server::Engine::call(&engine, &rest_context, EngineRequest::Memory)
        .await
        .unwrap();
    assert_eq!(listed.as_array().unwrap().len(), 1);
    assert_eq!(listed[0]["state"], "proposed");
    assert_eq!(listed[0]["kind"], "agent-finding");
    let accepted = knowell_server::Engine::call(
        &engine,
        &rest_context,
        EngineRequest::DecideMemory(MemoryDecision {
            id: written.record.id.to_string(),
            action: MemoryAction::Accept,
            note: None,
        }),
    )
    .await
    .unwrap();
    assert_eq!(accepted["state"], "accepted");
    let search_rest = knowell_server::Engine::call(
        &engine,
        &rest_context,
        EngineRequest::Search(
            serde_json::from_value(serde_json::json!({
                "query": "cancel subscription", "limit": 5
            }))
            .unwrap(),
        ),
    )
    .await
    .unwrap();
    assert!(!search_rest["results"].as_array().unwrap().is_empty());
    for key in ["queryClass", "tookMs", "skipped", "tokensReturned"] {
        assert!(search_rest.get(key).is_some(), "{key}");
    }
    let profiles = knowell_server::Engine::call(&engine, &rest_context, EngineRequest::Profiles)
        .await
        .unwrap();
    assert_eq!(profiles.as_array().unwrap().len(), 1, "{profiles}");
    let profile_id = profiles[0]["id"].as_str().unwrap().to_owned();
    let estimate = knowell_server::Engine::call(
        &engine,
        &rest_context,
        EngineRequest::SwitchEstimate {
            to_profile_id: profile_id,
        },
    )
    .await
    .unwrap();
    assert!(estimate["warnings"].is_array());
    let graph = knowell_server::Engine::call(
        &engine,
        &rest_context,
        EngineRequest::Graph(
            serde_json::from_value(serde_json::json!({"mode": "hierarchy"})).unwrap(),
        ),
    )
    .await
    .unwrap();
    assert_eq!(graph["nodes"].as_array().unwrap().len(), 10);
    for request in [
        EngineRequest::HealthDetail,
        EngineRequest::GraphInsights,
        EngineRequest::Domains,
        EngineRequest::Glossary,
        EngineRequest::Tasks,
        EngineRequest::Rules,
        EngineRequest::EvalReports,
        EngineRequest::Usage { days: 7 },
        EngineRequest::Integrations,
        EngineRequest::Admin,
    ] {
        knowell_server::Engine::call(&engine, &rest_context, request)
            .await
            .unwrap();
    }

    let usage =
        knowell_server::Engine::call(&engine, &rest_context, EngineRequest::Usage { days: 7 })
            .await
            .unwrap();
    let search_usage = usage["tools"]
        .as_array()
        .unwrap()
        .iter()
        .find(|t| t["tool"] == "search")
        .unwrap();
    assert!(search_usage["calls"].as_u64().unwrap() >= 1);
    assert!(usage["agents"][0]["agent"].is_string());

    // ------------------------------------------------------------- permissions
    // REST for a user who may only read billing-api.
    let bob_rest = EngineContext {
        principal: knowell_auth::Principal::User(crate::common::bob()),
        scopes: None,
        grants: Arc::new({
            let mut g = GrantSet::new();
            g.add(
                knowell_auth::Grant::new(
                    knowell_auth::Principal::User(crate::common::bob()),
                    knowell_auth::Role::Member,
                    knowell_auth::ResourceScope::project(name("acme-goods"), name("billing-api")),
                )
                .unwrap(),
            );
            g
        }),
        visible: knowell_auth::ProjectFilter::default(),
        request_id: knowell_auth::RequestId::new("req-2").unwrap(),
        audit: Arc::new(knowell_server::MemoryAuditSink::new()),
    };
    let bob_rest_search = knowell_server::Engine::call(
        &engine,
        &bob_rest,
        EngineRequest::Search(
            serde_json::from_value(
                serde_json::json!({"query": "cancel subscription", "limit": 50}),
            )
            .unwrap(),
        ),
    )
    .await
    .unwrap();
    let names: BTreeSet<&str> = bob_rest_search["results"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|r| r["projectName"].as_str())
        .collect();
    assert_eq!(names.into_iter().collect::<Vec<_>>(), ["billing-api"]);
    let bob_graph = knowell_server::Engine::call(
        &engine,
        &bob_rest,
        EngineRequest::Graph(
            serde_json::from_value(serde_json::json!({"mode": "hierarchy"})).unwrap(),
        ),
    )
    .await
    .unwrap();
    assert_eq!(bob_graph["nodes"].as_array().unwrap().len(), 1);
    let web_id = graph["nodes"]
        .as_array()
        .unwrap()
        .iter()
        .find(|n| n["label"] == "storefront-web")
        .and_then(|n| n["projectId"].as_str())
        .unwrap()
        .to_owned();
    let denied = knowell_server::Engine::call(
        &engine,
        &bob_rest,
        EngineRequest::Search(
            serde_json::from_value(serde_json::json!({"query": "cancel", "projectIds": [web_id]}))
                .unwrap(),
        ),
    )
    .await
    .unwrap_err();
    assert!(matches!(
        denied,
        knowell_server::EngineError::NotFound { .. }
    ));
    let bob_decide = knowell_server::Engine::call(
        &engine,
        &bob_rest,
        EngineRequest::DecideMemory(MemoryDecision {
            id: written.record.id.to_string(),
            action: MemoryAction::Reject,
            note: None,
        }),
    )
    .await
    .unwrap_err();
    assert!(
        matches!(bob_decide, knowell_server::EngineError::Forbidden { .. }),
        "a member may not review memory: {bob_decide:?}"
    );

    let bob_open = engine
        .open_workspace(&agent_b, OpenWorkspaceInput::default())
        .await
        .unwrap();
    let bob_projects: Vec<&str> = bob_open
        .manifest
        .iter()
        .map(|v| v.project.as_str())
        .collect();
    assert_eq!(bob_projects, ["billing-api"]);
    assert_eq!(bob_open.projects.len(), 1);
    let bob_ctx = context_target(&bob_open.context_id);
    let bob_search = engine
        .search(
            &agent_b,
            SearchInput {
                target: bob_ctx.clone(),
                query: "cancel subscription".into(),
                limit: Some(50),
                ..SearchInput::default()
            },
        )
        .await
        .unwrap();
    assert!(!bob_search.hits.is_empty());
    assert!(
        bob_search
            .hits
            .iter()
            .all(|h| h.evidence.project.as_str() == "billing-api")
    );
    assert!(bob_search.memory_hits.iter().all(|m| {
        m.record
            .scope
            .project
            .as_ref()
            .is_none_or(|p| p.as_str() == "billing-api")
    }));
    let bob_symbols = engine
        .inspect_symbol(
            &agent_b,
            InspectSymbolInput {
                target: bob_ctx.clone(),
                symbol: SymbolRef {
                    id: None,
                    symbol: Some("cancelSubscription".into()),
                    project: None,
                },
                include: Vec::new(),
                limit: None,
            },
        )
        .await
        .unwrap();
    assert!(
        bob_symbols
            .symbols
            .iter()
            .all(|s| s.definition.project.as_str() == "billing-api")
    );
    let web_hit = found
        .hits
        .iter()
        .find(|h| h.evidence.project.as_str() == "storefront-web")
        .unwrap();
    let bob_fetch = engine
        .fetch(
            &agent_b,
            FetchInput {
                target: bob_ctx.clone(),
                ids: vec![web_hit.id.clone()],
                ..FetchInput::default()
            },
        )
        .await
        .unwrap();
    assert!(bob_fetch.items.is_empty());
    assert!(
        bob_fetch
            .gaps
            .iter()
            .all(|g| g.reason == GapReason::NotFound)
    );
    let bob_pack = engine
        .build_context(
            &agent_b,
            BuildContextInput {
                target: bob_ctx.clone(),
                task: Some("cancel subscription in the web and mobile clients".into()),
                ..BuildContextInput::default()
            },
        )
        .await
        .unwrap();
    assert!(
        bob_pack
            .entries
            .iter()
            .filter_map(|e| e.evidence.as_ref())
            .all(|e| e.project.as_str() == "billing-api")
    );
    let bob_status = engine
        .index_status(
            &agent_b,
            IndexStatusInput {
                target: bob_ctx.clone(),
                ..IndexStatusInput::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(bob_status.projects.len(), 1);
    let bob_trace = engine
        .trace_flow(
            &agent_b,
            TraceFlowInput {
                target: bob_ctx.clone(),
                symbol: Some("cancelSubscription".into()),
                direction: Some(knowell_mcp::tools::FlowDirection::Both),
                ..TraceFlowInput::default()
            },
        )
        .await
        .unwrap();
    assert!(bob_trace.nodes.iter().all(|n| {
        n.project
            .as_ref()
            .is_none_or(|p| p.as_str() == "billing-api")
    }));
    let bob_impact = engine
        .analyze_impact(
            &agent_b,
            AnalyzeImpactInput {
                target: bob_ctx.clone(),
                change: Some(ChangeSubject::Symbol {
                    symbol: SymbolRef {
                        id: None,
                        symbol: Some("cancelSubscription".into()),
                        project: None,
                    },
                }),
                ..AnalyzeImpactInput::default()
            },
        )
        .await
        .unwrap();
    assert!(
        bob_impact
            .changed
            .iter()
            .chain(bob_impact.impacted.iter())
            .chain(bob_impact.tests.iter())
            .all(|i| i.evidence.project.as_str() == "billing-api")
    );
    let bob_memory = engine
        .read_memory(
            &agent_b,
            ReadMemoryInput {
                target: bob_ctx.clone(),
                ..ReadMemoryInput::default()
            },
        )
        .await
        .unwrap();
    assert!(bob_memory.records.iter().all(|r| {
        r.scope
            .project
            .as_ref()
            .is_some_and(|p| p.as_str() == "billing-api")
    }));
    let bob_contracts = engine
        .contracts(
            &agent_b,
            ContractsInput {
                target: bob_ctx.clone(),
                ..ContractsInput::default()
            },
        )
        .await
        .unwrap();
    assert!(
        bob_contracts
            .contracts
            .iter()
            .flat_map(|c| c.participants.iter())
            .all(|p| p.project.as_str() == "billing-api")
    );
    // Another identity's context is unknown, and so are invisible projects.
    let stolen = engine
        .search(
            &agent_b,
            SearchInput {
                target: ctx.clone(),
                query: "cancel".into(),
                ..SearchInput::default()
            },
        )
        .await
        .unwrap_err();
    assert_eq!(stolen.kind(), "not_found");
    let invisible = engine
        .open_workspace(
            &agent_b,
            OpenWorkspaceInput {
                views: vec![knowell_mcp::ViewPin {
                    project: name("storefront-web"),
                    view: "branch:main".parse().unwrap(),
                }],
                ..OpenWorkspaceInput::default()
            },
        )
        .await
        .unwrap_err();
    assert_eq!(invisible.kind(), "not_found");

    // ------------------------------------------------------- personal overlay
    let worktree = ws.dir.path().join("billing-wt");
    ws.git(
        "billing-api",
        &[
            "worktree",
            "add",
            "-q",
            "-b",
            "feature/grace",
            worktree.to_str().unwrap(),
        ],
    );
    let service = worktree.join("src/subscriptions/subscription.service.ts");
    let text = std::fs::read_to_string(&service).unwrap();
    std::fs::write(
        &service,
        text.replace(
            "async cancelSubscription(",
            "async cancelWithGracePeriodQuokka(): Promise<void> {}\n\n  async cancelSubscription(",
        ),
    )
    .unwrap();
    let personal = engine
        .open_workspace(
            &agent_a,
            OpenWorkspaceInput {
                working_directory: Some(worktree.to_string_lossy().into_owned()),
                ..OpenWorkspaceInput::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(
        personal.current_project.as_ref().map(|p| p.as_str()),
        Some("billing-api")
    );
    let billing_view = personal
        .manifest
        .iter()
        .find(|v| v.project.as_str() == "billing-api")
        .unwrap();
    assert_eq!(
        billing_view.layer,
        ViewLayer::Personal,
        "{:?}",
        personal.gaps
    );
    let personal_hits = engine
        .search(
            &agent_a,
            SearchInput {
                target: context_target(&personal.context_id),
                query: "cancelWithGracePeriodQuokka".into(),
                ..SearchInput::default()
            },
        )
        .await
        .unwrap();
    let hit = personal_hits
        .hits
        .first()
        .expect("the overlay symbol is found");
    assert_eq!(hit.evidence.layer, ViewLayer::Personal);
    assert!(
        personal_hits
            .hits
            .iter()
            .filter(|h| h.evidence.path.as_str() == "src/subscriptions/subscription.service.ts")
            .all(|h| h.evidence.layer == ViewLayer::Personal)
    );
    // Bob shares the project but not Alice's worktree.
    let bob_overlay = engine
        .search(
            &agent_b,
            SearchInput {
                target: bob_ctx.clone(),
                query: "cancelWithGracePeriodQuokka".into(),
                ..SearchInput::default()
            },
        )
        .await
        .unwrap();
    assert!(
        bob_overlay
            .hits
            .iter()
            .all(|h| h.evidence.layer == ViewLayer::Shared && !h.title.contains("Quokka"))
    );

    // ------------------------------------------------ resume after a commit
    let saved = engine
        .save_checkpoint(
            &agent_a,
            SaveCheckpointInput {
                target: ctx.clone(),
                goal: Some("Add a grace period to subscription cancellation".into()),
                progress: "Read SubscriptionService.cancelSubscription".into(),
                next_steps: vec!["change the service".into()],
                open_questions: vec!["does mobile need a new string?".into()],
                ..SaveCheckpointInput::default()
            },
        )
        .await
        .unwrap();
    assert!(saved.created_task && saved.created);
    let commit_file = ws
        .project_dir("billing-api")
        .join("src/subscriptions/subscription.service.ts");
    let original = std::fs::read_to_string(&commit_file).unwrap();
    std::fs::write(
        &commit_file,
        format!("{original}\n// grace period follows ADR-0005\n"),
    )
    .unwrap();
    let new_commit = ws.commit_all("billing-api", "note the grace period");
    engine
        .indexer()
        .index_workspace(&ws.resolved, Priority::Interactive)
        .await
        .unwrap();
    let reopened = engine
        .open_workspace(&agent_a, OpenWorkspaceInput::default())
        .await
        .unwrap();
    let reopened_billing = reopened
        .manifest
        .iter()
        .find(|v| v.project.as_str() == "billing-api")
        .unwrap();
    assert_eq!(
        reopened_billing.commit.as_ref().unwrap().as_str(),
        new_commit
    );
    let resumed = engine
        .resume_task(
            &agent_a,
            ResumeTaskInput {
                target: context_target(&reopened.context_id),
                task_id: Some(saved.task_id.clone()),
                ..ResumeTaskInput::default()
            },
        )
        .await
        .unwrap();
    let task = resumed.task.expect("the task resumes");
    assert!(
        task.changed_since
            .iter()
            .any(|c| c.project.as_str() == "billing-api"
                && c.path.as_str() == "src/subscriptions/subscription.service.ts"
                && c.to_commit.as_str() == new_commit),
        "{:?}",
        task.changed_since
    );
    assert_eq!(task.next_steps.len(), 1);
    assert_eq!(task.open_questions.len(), 1);
    let listed_tasks = engine
        .resume_task(
            &agent_a,
            ResumeTaskInput {
                target: context_target(&reopened.context_id),
                ..ResumeTaskInput::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(listed_tasks.tasks.len(), 1);
    // The old context still pins the old commit.
    let old_view = engine
        .index_status(
            &agent_a,
            IndexStatusInput {
                target: ctx.clone(),
                projects: vec![name("billing-api")],
                ..IndexStatusInput::default()
            },
        )
        .await
        .unwrap();
    assert_ne!(
        old_view.projects[0]
            .indexed_commit
            .as_ref()
            .unwrap()
            .as_str(),
        new_commit
    );

    // ------------------------------------------------------- evaluation hook
    let access = Access::new(
        knowell_auth::Principal::User(alice()),
        rest_context.grants.clone(),
    );
    let hybrid = HybridRetriever::new(&engine, &access, &name("acme-goods"))
        .await
        .unwrap();
    // The fixture changed (one commit); measure on the original files.
    let measured_root = tempfile::tempdir().unwrap();
    let original_root = measured_root.path().join("ws");
    ws.fixture
        .write_to(&original_root, &knowell_eval::WriteOptions { git: false })
        .unwrap();
    let walked = walk_fixture(&original_root, &ws.fixture).unwrap();
    let queries = QuerySet::builtin().unwrap();
    let grep = GrepRetriever::new(&walked.corpus);
    let bm25 = Bm25Retriever::new(&walked.corpus).unwrap();
    let retrievers: [&dyn Retriever; 3] = [&grep, &bm25, &hybrid];
    let report = run(&walked.corpus, &queries, &retrievers, 10).unwrap();
    eprintln!("{}", report.to_markdown());
    let hybrid_report = report.retriever("hybrid").unwrap();
    assert!(hybrid_report.overall.recall_at_10.unwrap_or(0.0) > 0.0);
}
