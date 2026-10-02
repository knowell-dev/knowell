use knowell_store::graph::*;
use knowell_store::symbols::{self, NewSymbol};
use knowell_store::views::{self, GenerationPin};
use knowell_store::{
    ContractKind, ContractRole, EvidenceType, ProjectId, Resolution, StoreError, SymbolId, ViewId,
};
use serde_json::json;

use crate::common::{add_project, fixture, path, require_db};

fn edge(from: NodeRef, to: NodeRef, kind: &str, origin: &str) -> NewEdge {
    NewEdge {
        from,
        to,
        kind: kind.into(),
        evidence_type: EvidenceType::Syntactic,
        resolution: Resolution::Resolved,
        evidence: json!({ "file": origin, "line": 1 }),
        origin: origin.into(),
    }
}

async fn symbols(c: &mut sqlx::PgConnection, project: ProjectId, names: &[&str]) -> Vec<SymbolId> {
    let new: Vec<NewSymbol> = names
        .iter()
        .map(|n| NewSymbol {
            qualified_name: (*n).to_owned(),
            kind: "function".into(),
        })
        .collect();
    symbols::upsert_symbols(c, project, &new).await.unwrap()
}

fn reached(walk: &Walk) -> Vec<(u32, NodeRef)> {
    walk.steps
        .iter()
        .map(|s| (s.depth, s.reached.clone()))
        .collect()
}

fn opts(depth: u32, direction: WalkDirection) -> WalkOptions {
    WalkOptions {
        max_depth: depth,
        direction,
        ..WalkOptions::default()
    }
}

#[tokio::test]
async fn walks_are_bounded_by_depth_kind_and_budget() {
    let db = require_db!();
    let mut c = db.conn().await;
    let fx = fixture(&mut c, "api").await;
    let view = fx.view.id;
    let ids = symbols(&mut c, fx.project.id, &["a", "b", "c", "d", "e", "x"]).await;
    let n = |i: usize| NodeRef::Symbol(ids[i]);
    let g1 = views::begin_generation(&mut c, view, None).await.unwrap();
    // a -> b -> c -> d -> e -> a (a cycle), plus a -imports-> x.
    let mut edges: Vec<NewEdge> = (0..5)
        .map(|i| edge(n(i), n((i + 1) % 5), "calls", "src/lib.rs"))
        .collect();
    edges.push(edge(n(0), n(5), "imports", "src/lib.rs"));
    let written = replace_edges(&mut c, view, g1, &["src/lib.rs".into()], &edges)
        .await
        .unwrap();
    assert_eq!(written, 6);
    views::activate_generation(&mut c, view, g1).await.unwrap();
    let pins = [GenerationPin {
        view,
        generation: 1,
    }];

    let w = walk_edges(&mut c, &n(0), &pins, &opts(2, WalkDirection::Outgoing))
        .await
        .unwrap();
    // Depth 1: b (calls) and x (imports); depth 2: c.
    let mut got = reached(&w);
    got.sort();
    let mut want = vec![(1, n(1)), (1, n(5)), (2, n(2))];
    want.sort();
    assert_eq!(got, want);
    assert!(!w.truncated);
    assert!(w.steps.windows(2).all(|p| p[0].depth <= p[1].depth));

    // Kind filter.
    let calls_only = WalkOptions {
        kinds: vec!["calls".into()],
        ..opts(3, WalkDirection::Outgoing)
    };
    let w = walk_edges(&mut c, &n(0), &pins, &calls_only).await.unwrap();
    assert_eq!(reached(&w), vec![(1, n(1)), (2, n(2)), (3, n(3))]);

    // The cycle never revisits the start node, even at the maximum depth.
    let w = walk_edges(
        &mut c,
        &n(0),
        &pins,
        &WalkOptions {
            kinds: vec!["calls".into()],
            ..opts(MAX_WALK_DEPTH, WalkDirection::Outgoing)
        },
    )
    .await
    .unwrap();
    assert_eq!(
        w.steps.len(),
        4,
        "a->b->c->d->e, the edge back to a is never taken"
    );
    assert!(w.steps.iter().all(|s| s.reached != n(0)));

    // Incoming: who reaches c within two hops?
    let w = walk_edges(&mut c, &n(2), &pins, &opts(2, WalkDirection::Incoming))
        .await
        .unwrap();
    assert_eq!(reached(&w), vec![(1, n(1)), (2, n(0))]);
    assert_eq!(w.steps[0].edge.edge.from, n(1));

    // Bounds are validated.
    for bad in [0, MAX_WALK_DEPTH + 1] {
        assert!(matches!(
            walk_edges(&mut c, &n(0), &pins, &opts(bad, WalkDirection::Outgoing)).await,
            Err(StoreError::InvalidInput(_))
        ));
    }

    // The edge budget truncates a wide fan-out.
    let fan: Vec<String> = (0..50).map(|i| format!("leaf{i}")).collect();
    let fan_refs: Vec<&str> = fan.iter().map(String::as_str).collect();
    let leaves = symbols(&mut c, fx.project.id, &fan_refs).await;
    let g2 = views::begin_generation(&mut c, view, None).await.unwrap();
    let fan_edges: Vec<NewEdge> = leaves
        .iter()
        .map(|l| edge(n(5), NodeRef::Symbol(*l), "calls", "src/fan.rs"))
        .collect();
    replace_edges(&mut c, view, g2, &["src/fan.rs".into()], &fan_edges)
        .await
        .unwrap();
    views::activate_generation(&mut c, view, g2).await.unwrap();
    let pins2 = [GenerationPin {
        view,
        generation: 2,
    }];
    let w = walk_edges(
        &mut c,
        &n(5),
        &pins2,
        &WalkOptions {
            max_edges: 10,
            ..opts(1, WalkDirection::Outgoing)
        },
    )
    .await
    .unwrap();
    assert_eq!(w.steps.len(), 10);
    assert!(w.truncated);
    // Generation 1 does not have the fan-out.
    let w = walk_edges(&mut c, &n(5), &pins, &opts(1, WalkDirection::Outgoing))
        .await
        .unwrap();
    assert!(w.steps.is_empty());
}

#[tokio::test]
async fn edges_are_generation_scoped_per_origin() {
    let db = require_db!();
    let mut c = db.conn().await;
    let fx = fixture(&mut c, "api").await;
    let view = fx.view.id;
    let ids = symbols(&mut c, fx.project.id, &["main", "parse", "render"]).await;
    let s = |i: usize| NodeRef::Symbol(ids[i]);
    let file = NodeRef::File {
        project: fx.project.id,
        path: path("src/main.rs"),
    };

    let g1 = views::begin_generation(&mut c, view, None).await.unwrap();
    replace_edges(
        &mut c,
        view,
        g1,
        &["src/main.rs".into()],
        &[
            edge(s(0), s(1), "calls", "src/main.rs"),
            edge(file.clone(), s(0), "defines", "src/main.rs"),
        ],
    )
    .await
    .unwrap();
    views::activate_generation(&mut c, view, g1).await.unwrap();

    // Generation 2 re-analyses main.rs: main now calls render instead.
    let g2 = views::begin_generation(&mut c, view, None).await.unwrap();
    let new = [
        edge(s(0), s(2), "calls", "src/main.rs"),
        edge(file.clone(), s(0), "defines", "src/main.rs"),
    ];
    replace_edges(&mut c, view, g2, &["src/main.rs".into()], &new)
        .await
        .unwrap();
    // Retry within the same generation replaces the earlier attempt.
    replace_edges(&mut c, view, g2, &["src/main.rs".into()], &new)
        .await
        .unwrap();
    // Edges outside the replaced origins are refused.
    assert!(matches!(
        replace_edges(&mut c, view, g2, &["src/a.rs".into()], &new).await,
        Err(StoreError::InvalidInput(_))
    ));
    views::activate_generation(&mut c, view, g2).await.unwrap();

    let at = |g| {
        [GenerationPin {
            view,
            generation: g,
        }]
    };
    let callee = |edges: Vec<Edge>| -> Vec<NodeRef> {
        edges
            .into_iter()
            .filter(|e| e.edge.kind == "calls")
            .map(|e| e.edge.to)
            .collect()
    };
    assert_eq!(
        callee(edges_at(&mut c, &s(0), &at(1)).await.unwrap()),
        vec![s(1)]
    );
    assert_eq!(
        callee(edges_at(&mut c, &s(0), &at(2)).await.unwrap()),
        vec![s(2)]
    );
    let defines = edges_at(&mut c, &file, &at(2)).await.unwrap();
    assert_eq!(defines.len(), 1);
    assert_eq!(defines[0].edge.evidence["file"], "src/main.rs");
    assert_eq!(defines[0].valid_from, 2);
    let total: i64 = sqlx::query_scalar("SELECT count(*) FROM edge WHERE view_id = $1")
        .bind(view)
        .fetch_one(&mut *c)
        .await
        .unwrap();
    assert_eq!(
        total, 4,
        "two closed in generation 2, two current; retries left no extras"
    );
}

#[tokio::test]
async fn contracts_link_producers_and_consumers_across_projects() {
    let db = require_db!();
    let mut c = db.conn().await;
    let fx = fixture(&mut c, "orders").await;
    let (billing, billing_view) = add_project(&mut c, &fx, "billing").await;
    let publisher = symbols(&mut c, fx.project.id, &["OrderService::place"]).await[0];
    let handler = symbols(&mut c, billing.id, &["OnOrderCreated::handle"]).await[0];
    let topic = NodeRef::Contract {
        workspace: fx.workspace.id,
        kind: ContractKind::Topic,
        key: "orders.created".into(),
    };

    let db = &db;
    let write = |view: ViewId, role: ContractRole, symbol: SymbolId, origin: &str| {
        let origin = origin.to_owned();
        let topic = topic.clone();
        let edge_kind = match role {
            ContractRole::Producer => "produces",
            ContractRole::Consumer => "consumes",
        };
        async move {
            let mut c = db.conn().await;
            let g = views::begin_generation(&mut c, view, None).await.unwrap();
            replace_edges(
                &mut c,
                view,
                g,
                std::slice::from_ref(&origin),
                &[edge(NodeRef::Symbol(symbol), topic, edge_kind, &origin)],
            )
            .await
            .unwrap();
            replace_contracts(
                &mut c,
                view,
                g,
                std::slice::from_ref(&origin),
                &[NewContract {
                    kind: ContractKind::Topic,
                    key: "orders.created".into(),
                    role,
                    origin: origin.clone(),
                    symbol: Some(symbol),
                    evidence_type: EvidenceType::ContractDerived,
                    evidence: json!({ "schema": "proto/orders.proto" }),
                }],
            )
            .await
            .unwrap();
            views::activate_generation(&mut c, view, g).await.unwrap();
        }
    };
    write(
        fx.view.id,
        ContractRole::Producer,
        publisher,
        "src/orders.rs",
    )
    .await;
    write(
        billing_view.id,
        ContractRole::Consumer,
        handler,
        "src/handlers.rs",
    )
    .await;

    let both = [
        GenerationPin {
            view: fx.view.id,
            generation: 1,
        },
        GenerationPin {
            view: billing_view.id,
            generation: 1,
        },
    ];
    // Producer -> topic <- consumer: reachable when walking both ways across
    // the two projects' pins.
    let w = walk_edges(
        &mut c,
        &NodeRef::Symbol(publisher),
        &both,
        &opts(2, WalkDirection::Both),
    )
    .await
    .unwrap();
    assert_eq!(
        reached(&w),
        vec![(1, topic.clone()), (2, NodeRef::Symbol(handler))]
    );
    // Pinned to the producer's project only, the consumer is invisible.
    let w = walk_edges(
        &mut c,
        &NodeRef::Symbol(publisher),
        &both[..1],
        &opts(2, WalkDirection::Both),
    )
    .await
    .unwrap();
    assert_eq!(reached(&w), vec![(1, topic.clone())]);

    let parties = contract_parties(&mut c, &both, ContractKind::Topic, "orders.created")
        .await
        .unwrap();
    let summary: Vec<(ContractRole, ProjectId, Option<SymbolId>)> = parties
        .iter()
        .map(|p| (p.contract.role, p.project, p.contract.symbol))
        .collect();
    assert_eq!(
        summary,
        vec![
            (ContractRole::Producer, fx.project.id, Some(publisher)),
            (ContractRole::Consumer, billing.id, Some(handler)),
        ]
    );
    assert_eq!(
        parties[0].contract.evidence_type,
        EvidenceType::ContractDerived
    );
    assert!(
        contract_parties(&mut c, &both, ContractKind::Topic, "orders.cancelled")
            .await
            .unwrap()
            .is_empty()
    );
}

#[tokio::test]
async fn unresolved_and_ambiguous_targets_stay_visible() {
    let db = require_db!();
    let mut c = db.conn().await;
    let fx = fixture(&mut c, "web").await;
    let view = fx.view.id;
    let ids = symbols(
        &mut c,
        fx.project.id,
        &["App::load", "api::get", "cache::get"],
    )
    .await;
    let g = views::begin_generation(&mut c, view, None).await.unwrap();
    let mut unresolved = edge(
        NodeRef::Symbol(ids[0]),
        NodeRef::Name {
            project: fx.project.id,
            name: "window.fetchJson".into(),
        },
        "calls",
        "src/app.ts",
    );
    unresolved.resolution = Resolution::Unresolved;
    let mut candidates: Vec<NewEdge> = [ids[1], ids[2]]
        .iter()
        .map(|t| {
            let mut e = edge(
                NodeRef::Symbol(ids[0]),
                NodeRef::Symbol(*t),
                "calls",
                "src/app.ts",
            );
            e.resolution = Resolution::Ambiguous;
            e.evidence_type = EvidenceType::Heuristic;
            e
        })
        .collect();
    candidates.push(unresolved);
    replace_edges(&mut c, view, g, &["src/app.ts".into()], &candidates)
        .await
        .unwrap();
    views::activate_generation(&mut c, view, g).await.unwrap();
    let edges = edges_at(
        &mut c,
        &NodeRef::Symbol(ids[0]),
        &[GenerationPin {
            view,
            generation: g,
        }],
    )
    .await
    .unwrap();
    let mut kinds: Vec<(Resolution, EvidenceType)> = edges
        .iter()
        .map(|e| (e.edge.resolution, e.edge.evidence_type))
        .collect();
    kinds.sort();
    assert_eq!(
        kinds,
        vec![
            (Resolution::Ambiguous, EvidenceType::Heuristic),
            (Resolution::Ambiguous, EvidenceType::Heuristic),
            (Resolution::Unresolved, EvidenceType::Syntactic),
        ]
    );
    assert!(edges.iter().any(|e| matches!(
        &e.edge.to,
        NodeRef::Name { name, .. } if name == "window.fetchJson"
    )));
}

#[tokio::test]
async fn edges_and_contracts_are_found_by_target_name_tail_and_origin() {
    let db = require_db!();
    let mut c = db.conn().await;
    let fx = fixture(&mut c, "api").await;
    let view = fx.view.id;
    let project = fx.project.id;
    let ids = symbols(&mut c, project, &["src/util.ts#helper", "src/main.ts#run"]).await;
    let file = |p: &str| NodeRef::File {
        project,
        path: path(p),
    };
    let name = |n: &str| NodeRef::Name {
        project,
        name: n.to_owned(),
    };
    let g1 = views::begin_generation(&mut c, view, None).await.unwrap();
    let main_edges = vec![
        edge(
            file("src/main.ts"),
            file("src/util.ts"),
            "imports",
            "src/main.ts",
        ),
        edge(
            file("src/main.ts"),
            name("./later"),
            "imports",
            "src/main.ts",
        ),
        edge(file("src/main.ts"), name("react"), "imports", "src/main.ts"),
        edge(
            file("src/main.ts"),
            name("../lib/later.ts"),
            "imports",
            "src/main.ts",
        ),
        edge(
            NodeRef::Symbol(ids[1]),
            NodeRef::Symbol(ids[0]),
            "references",
            "src/main.ts",
        ),
    ];
    replace_edges(&mut c, view, g1, &["src/main.ts".into()], &main_edges)
        .await
        .unwrap();
    let other = vec![edge(
        file("src/other.ts"),
        file("src/util.ts"),
        "imports",
        "src/other.ts",
    )];
    replace_edges(&mut c, view, g1, &["src/other.ts".into()], &other)
        .await
        .unwrap();
    let contract = NewContract {
        kind: ContractKind::Endpoint,
        key: "GET /v1/x".into(),
        role: ContractRole::Consumer,
        origin: "link:src/main.ts".into(),
        symbol: Some(ids[1]),
        evidence_type: EvidenceType::Syntactic,
        evidence: json!({ "path": "src/main.ts" }),
    };
    replace_contracts(
        &mut c,
        view,
        g1,
        &["link:src/main.ts".into()],
        std::slice::from_ref(&contract),
    )
    .await
    .unwrap();
    views::activate_generation(&mut c, view, g1).await.unwrap();
    let pin = GenerationPin {
        view,
        generation: g1,
    };

    // Importers of a file, referrers of a symbol.
    let importers = edges_into(&mut c, pin, "imports", &[file("src/util.ts")])
        .await
        .unwrap();
    let mut from: Vec<NodeRef> = importers.iter().map(|e| e.edge.from.clone()).collect();
    from.sort();
    assert_eq!(from, vec![file("src/main.ts"), file("src/other.ts")]);
    assert!(
        edges_into(&mut c, pin, "calls", &[file("src/util.ts")])
            .await
            .unwrap()
            .is_empty()
    );
    let referrers = edges_into(&mut c, pin, "references", &[NodeRef::Symbol(ids[0])])
        .await
        .unwrap();
    assert_eq!(referrers.len(), 1);
    assert_eq!(referrers[0].edge.origin, "src/main.ts");

    // Unresolved names by their last path segment.
    let tails = edges_into_name_tails(
        &mut c,
        pin,
        "imports",
        project,
        &["later".to_owned(), "later.ts".to_owned()],
    )
    .await
    .unwrap();
    let mut names: Vec<NodeRef> = tails.iter().map(|e| e.edge.to.clone()).collect();
    names.sort();
    assert_eq!(names, vec![name("../lib/later.ts"), name("./later")]);
    assert!(
        edges_into_name_tails(&mut c, pin, "imports", project, &[])
            .await
            .unwrap()
            .is_empty()
    );
    assert_eq!(
        edges_into_name_tails(&mut c, pin, "imports", project, &["react".to_owned()])
            .await
            .unwrap()
            .len(),
        1
    );

    // What an origin wrote.
    let by_origin = edges_with_origins(&mut c, pin, &["src/other.ts".into()])
        .await
        .unwrap();
    assert_eq!(by_origin.len(), 1);
    assert_eq!(by_origin[0].edge, other[0]);
    let parties = contracts_with_origins(&mut c, pin, &["link:src/main.ts".into()])
        .await
        .unwrap();
    assert_eq!(parties.len(), 1);
    assert_eq!(parties[0].contract, contract);
    assert_eq!(parties[0].project, project);
    assert!(
        contracts_with_origins(&mut c, pin, &["src/main.ts".into()])
            .await
            .unwrap()
            .is_empty()
    );
}

#[tokio::test]
async fn definitions_are_listed_per_file_and_generation() {
    let db = require_db!();
    let mut c = db.conn().await;
    let fx = fixture(&mut c, "api").await;
    let view = fx.view.id;
    let ids = symbols(
        &mut c,
        fx.project.id,
        &["a.ts#one", "a.ts#two", "b.ts#three"],
    )
    .await;
    let occurrence = |symbol, p: &str, line| symbols::NewOccurrence {
        symbol,
        path: path(p),
        content_hash: knowell_core::ContentHash::of(p.as_bytes()),
        lines: knowell_core::LineRange::new(line, line).unwrap(),
        role: knowell_store::OccurrenceRole::Definition,
    };
    let g1 = views::begin_generation(&mut c, view, None).await.unwrap();
    let mut written = vec![
        occurrence(ids[1], "a.ts", 5),
        occurrence(ids[0], "a.ts", 1),
        occurrence(ids[2], "b.ts", 1),
    ];
    // A reference is not a definition.
    written.push(symbols::NewOccurrence {
        role: knowell_store::OccurrenceRole::Reference,
        ..occurrence(ids[2], "a.ts", 7)
    });
    symbols::replace_occurrences(&mut c, view, g1, &[path("a.ts"), path("b.ts")], &written)
        .await
        .unwrap();
    views::activate_generation(&mut c, view, g1).await.unwrap();
    let pin = GenerationPin {
        view,
        generation: g1,
    };
    let defs = symbols::definitions_in_paths(&mut c, pin, &[path("a.ts"), path("a.ts")])
        .await
        .unwrap();
    let names: Vec<(&str, u32)> = defs
        .iter()
        .map(|d| (d.symbol.qualified_name.as_str(), d.lines.start()))
        .collect();
    assert_eq!(names, [("a.ts#one", 1), ("a.ts#two", 5)]);
    assert_eq!(
        symbols::definitions_in_paths(&mut c, pin, &[path("b.ts"), path("none.ts")])
            .await
            .unwrap()
            .len(),
        1
    );
    let earlier = GenerationPin {
        view: ViewId(uuid::Uuid::now_v7()),
        generation: 1,
    };
    assert!(
        symbols::definitions_in_paths(&mut c, earlier, &[path("a.ts")])
            .await
            .unwrap()
            .is_empty()
    );
}
