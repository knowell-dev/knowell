use std::cmp::Ordering;
use std::collections::BTreeSet;

use serde::{Deserialize, Serialize};

use crate::scope::Rejection;
use crate::{
    CommitId, Component, Degradation, ExpansionConfig, Language, Layer, Location, QueryScope,
    Reason, SearchResult, SourceError,
};

/// Relation of a neighbour to the node it was reached from.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EdgeKind {
    /// The neighbour calls the node.
    Caller,
    /// The node calls the neighbour.
    Callee,
    /// A type the node uses, defines or implements.
    Type,
    /// A test that references the node.
    Test,
    /// A contract the node exposes or consumes (endpoint, topic, RPC, table,
    /// env name, i18n key), or the other side of that contract.
    Contract,
    /// Documentation (ADR, KNOWLEDGE.md, README section) about the node.
    Doc,
}

/// How an edge is known, strongest first (see ARCHITECTURE §7.1). Never
/// merged with [`Resolution`] into one number.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EvidenceType {
    /// Verified by a compiler, SCIP or a language tool.
    SemanticallyResolved,
    /// From OpenAPI, proto, a schema or a package manifest.
    ContractDerived,
    /// Observed at runtime in a specific version and environment.
    RuntimeObservation,
    /// Seen in source structure.
    SyntacticObservation,
    /// Name, structure or pattern similarity.
    HeuristicMatch,
    /// Proposed by a model; needs verification.
    ModelSuggestion,
}

/// Whether the edge's target is uniquely resolved.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Resolution {
    /// Exactly one target.
    Resolved,
    /// Several possible targets (dynamic dispatch, overloads, …).
    Ambiguous,
    /// The target could not be resolved.
    Unresolved,
}

/// A graph node: a located symbol or file region.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct GraphNode {
    /// Where it is.
    pub location: Location,
    /// Its symbol, if any.
    pub symbol: Option<String>,
    /// Its language, if known.
    pub language: Option<Language>,
}

/// A neighbour returned by a [`GraphExpander`].
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Neighbor {
    /// The neighbour.
    pub node: GraphNode,
    /// How it relates to the node it was asked for.
    pub edge: EdgeKind,
    /// How the edge is known.
    pub evidence: EvidenceType,
    /// Whether the edge's target is resolved.
    pub resolution: Resolution,
}

/// What a [`GraphExpander`] is asked.
#[derive(Clone, Copy, Debug)]
pub struct ExpandRequest<'a> {
    /// The node to expand.
    pub node: &'a GraphNode,
    /// Edge kinds wanted; others are ignored by the engine.
    pub edges: &'a [EdgeKind],
    /// Maximum neighbours wanted.
    pub limit: usize,
    /// Scope and pinned manifest; neighbours outside it are dropped.
    pub scope: &'a QueryScope,
}

/// Graph access for expansion (callers, callees, types, tests, contracts, docs).
pub trait GraphExpander {
    /// Neighbours of one node over the requested edge kinds.
    fn neighbors(&self, request: &ExpandRequest<'_>) -> Result<Vec<Neighbor>, SourceError>;
}

/// One edge walked from a seed.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct GraphStep {
    /// Edge kind.
    pub edge: EdgeKind,
    /// How the edge is known.
    pub evidence: EvidenceType,
    /// Whether its target is resolved.
    pub resolution: Resolution,
    /// The node reached.
    pub to: Location,
    /// Symbol of the node reached, if any.
    pub symbol: Option<String>,
}

impl GraphStep {
    /// Whether the step rests on weak evidence (heuristic or model
    /// suggestion) or an unresolved / ambiguous target.
    pub fn is_uncertain(&self) -> bool {
        matches!(
            self.evidence,
            EvidenceType::HeuristicMatch | EvidenceType::ModelSuggestion
        ) || self.resolution != Resolution::Resolved
    }
}

/// An item reached by graph expansion from a result.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ExpandedItem {
    /// Where it is.
    pub location: Location,
    /// Pinned commit of its view, if any.
    pub commit: Option<CommitId>,
    /// Base view or personal overlay.
    pub layer: Layer,
    /// Its symbol, if any.
    pub symbol: Option<String>,
    /// Its language, if known.
    pub language: Option<Language>,
    /// Rank of the result the path starts at.
    pub seed_rank: u32,
    /// Hops from the seed (≥ 1).
    pub depth: u32,
    /// Seed's fused score × decay ^ depth; orders expanded items, nothing more.
    pub score: f64,
    /// The edges walked from the seed, in order.
    pub path: Vec<GraphStep>,
    /// Why it is included (graph path, test reference).
    pub why: Vec<Reason>,
}

/// Graph expansion counters.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExpansionStats {
    /// The node budget in effect.
    pub node_budget: usize,
    /// Expander calls made.
    pub requests: usize,
    /// Expanded items added.
    pub added: usize,
    /// Neighbours that were already results; the result got a graph reason.
    pub annotated: usize,
    /// Neighbours already reached by another path.
    pub duplicates: usize,
    /// Neighbours over an edge kind that was not requested.
    pub ignored_edges: usize,
    /// Neighbours outside the pinned views.
    pub dropped_outside_pinned_views: usize,
    /// Neighbours outside the scope's filters.
    pub dropped_by_scope: usize,
    /// Base-view neighbours shadowed by the personal overlay.
    pub dropped_shadowed: usize,
    /// Neighbours not added because the node budget was used up.
    pub skipped_by_budget: usize,
    /// Whether expansion stopped because of the node budget.
    pub budget_exhausted: bool,
}

pub(crate) struct ExpansionOutcome {
    pub(crate) items: Vec<ExpandedItem>,
    pub(crate) stats: ExpansionStats,
    pub(crate) degraded: Option<Degradation>,
}

struct Frontier {
    node: GraphNode,
    seed: usize,
    path: Vec<GraphStep>,
}

/// Deterministic neighbour order, preferring stronger evidence when the
/// fanout truncates.
fn neighbor_order(a: &Neighbor, b: &Neighbor) -> Ordering {
    (
        a.edge,
        a.evidence,
        a.resolution,
        &a.node.location,
        &a.node.symbol,
    )
        .cmp(&(
            b.edge,
            b.evidence,
            b.resolution,
            &b.node.location,
            &b.node.symbol,
        ))
}

/// Breadth-first expansion from the top results, bounded by depth, edge
/// kinds, fanout and a total node budget. Neighbours that are already results
/// annotate those results with the graph path instead of being repeated.
pub(crate) fn expand(
    results: &mut [SearchResult],
    scope: &QueryScope,
    expander: &dyn GraphExpander,
    edges: &[EdgeKind],
    config: &ExpansionConfig,
) -> ExpansionOutcome {
    let mut stats = ExpansionStats {
        node_budget: config.node_budget,
        ..ExpansionStats::default()
    };
    let mut items: Vec<ExpandedItem> = Vec::new();
    let mut degraded = None;
    let mut visited: BTreeSet<Location> = BTreeSet::new();
    let mut annotated: BTreeSet<(usize, usize)> = BTreeSet::new();
    let mut frontier: Vec<Frontier> = results
        .iter()
        .take(config.seeds)
        .enumerate()
        .map(|(seed, r)| Frontier {
            node: GraphNode {
                location: r.location.clone(),
                symbol: r.symbol.clone(),
                language: r.language.clone(),
            },
            seed,
            path: Vec::new(),
        })
        .collect();

    'levels: for depth in 1..=config.max_depth {
        let mut next = Vec::new();
        for current in &frontier {
            if items.len() >= config.node_budget {
                stats.budget_exhausted = true;
                break 'levels;
            }
            let Some((seed_location, seed_rank, seed_score)) = results
                .get(current.seed)
                .map(|s| (s.location.clone(), s.rank, s.score.fused))
            else {
                continue;
            };
            let request = ExpandRequest {
                node: &current.node,
                edges,
                limit: config.fanout,
                scope,
            };
            stats.requests = stats.requests.saturating_add(1);
            let mut neighbors = match expander.neighbors(&request) {
                Ok(neighbors) => neighbors,
                Err(error) => {
                    degraded = Some(Degradation::new(Component::Graph, error.to_string()));
                    break 'levels;
                }
            };
            let before = neighbors.len();
            neighbors.retain(|n| edges.contains(&n.edge));
            stats.ignored_edges = stats.ignored_edges.saturating_add(before - neighbors.len());
            neighbors.sort_by(neighbor_order);
            neighbors.truncate(config.fanout);

            for neighbor in neighbors {
                let location = &neighbor.node.location;
                let admitted = scope.admit(
                    &location.project,
                    &location.view,
                    location.generation,
                    &location.path,
                    neighbor.node.language.as_ref(),
                );
                let layer = match admitted {
                    Err(Rejection::UnpinnedProject | Rejection::ViewMismatch) => {
                        stats.dropped_outside_pinned_views += 1;
                        continue;
                    }
                    Err(_) => {
                        stats.dropped_by_scope += 1;
                        continue;
                    }
                    Ok(Layer::Base)
                        if scope.declared_shadowed(&location.project, &location.path) =>
                    {
                        stats.dropped_shadowed += 1;
                        continue;
                    }
                    Ok(layer) => layer,
                };
                let step = GraphStep {
                    edge: neighbor.edge,
                    evidence: neighbor.evidence,
                    resolution: neighbor.resolution,
                    to: location.clone(),
                    symbol: neighbor.node.symbol.clone(),
                };
                let mut path = current.path.clone();
                path.push(step);
                let mut why = vec![Reason::GraphPath {
                    seed: seed_location.clone(),
                    steps: path.clone(),
                }];
                if neighbor.edge == crate::EdgeKind::Test {
                    let subject = current
                        .node
                        .symbol
                        .clone()
                        .unwrap_or_else(|| current.node.location.label());
                    why.push(Reason::TestReferences { subject });
                }

                if let Some(position) = results.iter().position(|r| r.location.overlaps(location)) {
                    if position != current.seed
                        && annotated.insert((position, current.seed))
                        && let Some(result) = results.get_mut(position)
                    {
                        result.why.extend(why);
                        stats.annotated += 1;
                    }
                    continue;
                }
                if !visited.insert(location.clone())
                    || items.iter().any(|i| i.location.overlaps(location))
                {
                    stats.duplicates += 1;
                    continue;
                }
                if items.len() >= config.node_budget {
                    stats.budget_exhausted = true;
                    stats.skipped_by_budget += 1;
                    continue;
                }
                let commit = scope
                    .pinned(&location.project, &location.view)
                    .and_then(|(pin, _)| pin.commit.clone());
                let exponent = i32::try_from(depth).unwrap_or(i32::MAX);
                items.push(ExpandedItem {
                    location: location.clone(),
                    commit,
                    layer,
                    symbol: neighbor.node.symbol.clone(),
                    language: neighbor.node.language.clone(),
                    seed_rank,
                    depth,
                    score: seed_score * config.score_decay.powi(exponent),
                    path: path.clone(),
                    why,
                });
                stats.added += 1;
                next.push(Frontier {
                    node: neighbor.node,
                    seed: current.seed,
                    path,
                });
            }
        }
        frontier = next;
        if frontier.is_empty() {
            break;
        }
    }
    ExpansionOutcome {
        items,
        stats,
        degraded,
    }
}
