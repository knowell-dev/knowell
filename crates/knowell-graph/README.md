# knowell-graph

The code and contract graph of Knowell. It holds the symbols, files, tests, docs and
decisions of every indexed project together with **contracts** (HTTP endpoints, topics, RPCs,
tables, env names, i18n keys, packages, infrastructure) as first-class nodes, so a UI call, the
controller that serves it, the event it emits, the worker that consumes it and the table that
worker writes are one connected path across repositories.

The crate is pure in-memory logic (`petgraph::StableGraph` plus an id index). It does no I/O and
does not depend on storage, parsing or query crates; those feed it [`GraphDelta`]s and ask it
questions.

## Model

- `NodeId`: a stable string key. Constructors give the conventional shapes
  (`NodeId::symbol(&project, key)`, `NodeId::contract(kind, key)`, ...). Contract ids carry no
  project: the same `(kind, key)` seen from two projects is one node, which is what links them.
- `Node`: `id`, `kind: NodeKind` (`Project`, `File`, `Symbol`, `Contract { kind, key }`, `Test`,
  `Doc`, `Decision`), `project: Option<Name>`, `generation` (view generation of the delta that
  last wrote it), `name`, optional `source: EvidenceRef`, and string attributes.
- `Edge`: `kind: EdgeKind`, `evidence: EvidenceType`, `resolution: Resolution`,
  `evidence_refs: Vec<EvidenceRef>` and attributes. An edge `a --kind--> b` reads "a *kind* b".
  Evidence type and resolution are separate axes and are **never merged into one confidence
  number**.
- `EvidenceRef`: `{ project, path, range, content_hash }`.

Evidence strength, strongest first: `SemanticResolved`, `RuntimeObserved`, `ContractDerived`,
`Syntactic`, `Heuristic`, `ModelSuggestion`. Resolution: `Resolved`, `Ambiguous`, `Unresolved`.

### Conventions the analyses rely on

| Relation | Edge |
|---|---|
| client calls endpoint / RPC | `client --Consumes--> contract` |
| controller serves endpoint | `controller --Exposes--> contract` |
| publisher emits event | `publisher --Produces--> topic` |
| subscriber receives event | `subscriber --Consumes--> topic` |
| code writes / reads a table | `code --Writes/Reads--> table` |
| code reads an env name | `code --Reads--> env name` |
| config declares env name, locale file defines key | `file --Defines--> contract` |
| code uses i18n key | `code --References--> key` |
| test covers code | `test --Tests--> subject` |

Attributes with meaning: `schema_hash` (contract node: the producer's current shape; on a
`Consumes`/`Reads` edge: the shape the consumer was built against), `locale` (on a locale file
node), `external` (`"true"` on a contract used from outside the workspace; insights skip it) and
`unresolved` (`"true"` on a placeholder node that is the target of an unresolved reference).

## Incremental updates and fencing

```rust
let mut graph = CodeGraph::new();
let summary = graph.apply(GraphDelta::new(project, generation)
    .add_node(node)
    .add_edge(&from, &to, edge))?;
```

`apply` mirrors storage per project view. It is atomic (a failing delta changes nothing) and
**fenced**: a delta whose generation is not strictly greater than the graph's generation for
that project is rejected with `GraphError::StaleGeneration`, so a late-finishing old job can
never overwrite a newer view. The first delta of a project uses generation 1 or higher.
Ownership is enforced: a delta may only add or remove nodes of its own project or unowned
nodes (contracts, decisions), and only edges with at least one endpoint in its project (or
two unowned endpoints). Removing a node removes every edge touching it, whoever owns the edge.

## Queries

- `neighbors(node, direction, &EdgeFilter)` and `walk(start, &WalkSpec)`: bounded breadth-first
  exploration. Depth `1..=5`, a node budget, and an `EdgeFilter` (kinds, minimum evidence,
  allow ambiguous, allow unresolved). Each visit carries the shortest path that reached it.
- `trace_flow(&FlowSpec)`: the k best paths from a node to another node, or all maximal
  flows from a node, downstream or upstream, across contract nodes (depth up to 10).
- `impact(&ImpactSpec)`: reverse reachability with per-node risk, tests, grouping by project
  and an explicit "unknown impact" list (depth up to 8).
- `insights(&InsightConfig)`: checks for `know check`.

### Flow orientation

Stored edge direction is not always the direction of flow. `trace_flow` uses: `Calls`,
`References`, `Produces`, `Writes` along the edge; `Consumes` onto a topic *against* the edge
(topic to subscriber) but along it for endpoints and RPCs (request); `Exposes` and `Reads`
against the edge (contract to controller, table to reader). Other kinds are not flow.

### Path ranking

Compared left to right: (1) weakest evidence on the path, stronger first; (2) number of
ambiguous or unresolved edges, fewer first; (3) length, shorter first (open-ended traces:
longer first); (4) sum of evidence ranks; (5) edge key sequence. Ambiguous and unresolved
edges are allowed but the path is flagged (`FlowPath::is_flagged`). Search is a bounded DFS
with an expansion budget; `FlowTrace::truncated` says when the budget stopped it.

### Impact and risk

A change propagates against most edges (a caller is affected when its callee changes) and
along the provider side of contracts (`Exposes`, `Produces`, `Writes`). A node reached from a
provider does not propagate back over provider edges, so changing one controller does not
"impact" another controller exposing the same endpoint. Tests, docs and decisions are reported
but not expanded. Each node carries its best carrying path and a level: `High` (strong,
resolved, at most two hops), `Medium` (syntactic, or strong over three or more hops), `Low`
(heuristic, model-suggested or ambiguous) and `Unknown` (an unresolved edge on the path).
Unresolved references whose placeholder shares a name with a changed node are listed as
`NameMatchesUnresolvedReference`.

## Insights

| Code | Severity | Meaning |
|---|---|---|
| `graph.endpoint_without_client` | info | endpoint with no `Consumes` edge |
| `graph.topic_without_consumer` | warning | topic with no `Consumes` edge |
| `graph.topic_without_producer` | warning | topic with no `Produces` edge |
| `graph.table_never_read` | info | table with no `Reads` edge |
| `graph.contract_drift` | error | consumer's `schema_hash` differs from the contract's |
| `graph.i18n_key_undefined` | error | key used but defined in no locale file |
| `graph.i18n_key_missing_locale` | error | key used but missing from a required locale |
| `graph.i18n_key_unused` | info | key defined but never used |
| `graph.env_undeclared` | warning | env name read in code but with no `Defines` edge |

Insights assume every project that uses a contract is indexed. Mark a contract used from
outside the workspace with the `external` attribute to skip it. A missing hash on either side
is "unknown", never drift.

## Determinism

Every returned collection is sorted and every comparison has an explicit tie-break; results do
not depend on insertion order or hashing. This is covered by tests that build the same graph in
reverse order.

## Limits

- Walks and flows clone node ids; this is fine for the 1-5 hop neighbourhoods Knowell serves,
  not for whole-graph analytics.
- `trace_flow` is a bounded DFS; on dense graphs raise `expansion_budget` or lower `max_depth`.
