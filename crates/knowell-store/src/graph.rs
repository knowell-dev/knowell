//! Evidence-carrying edges, contract participation, and bounded traversal.

use std::collections::BTreeSet;

use knowell_core::RepoPath;
use sqlx::{Connection, PgConnection};
use uuid::Uuid;

use crate::content::{BATCH_ROWS, split_pins};
use crate::error::{StoreError, map_write};
use crate::hierarchy::stored_path;
use crate::ids::{ContractId, EdgeId, ProjectId, SymbolId, ViewId, WorkspaceId};
use crate::symbols::{Scoped, replace_scope};
use crate::types::{ContractKind, ContractRole, EvidenceType, NodeKind, Resolution};
use crate::views::{GenerationPin, lock_building};

/// Deepest traversal [`walk_edges`] accepts.
pub const MAX_WALK_DEPTH: u32 = 8;
/// Most edges [`walk_edges`] returns.
pub const MAX_WALK_EDGES: u32 = 10_000;

/// A graph node an edge endpoint refers to.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum NodeRef {
    /// A logical symbol.
    Symbol(SymbolId),
    /// A file of a project.
    File {
        /// Project.
        project: ProjectId,
        /// Path in the project's views.
        path: RepoPath,
    },
    /// A whole project.
    Project(ProjectId),
    /// A contract of a workspace, shared by its producers and consumers.
    Contract {
        /// Workspace the contract lives in.
        workspace: WorkspaceId,
        /// Kind of contract.
        kind: ContractKind,
        /// Contract key (`GET /orders/{id}`, `orders.created`, ...).
        key: String,
    },
    /// A target known only by name: unresolved, or outside the indexed code.
    Name {
        /// Project the name was seen in.
        project: ProjectId,
        /// The name as written.
        name: String,
    },
}

impl NodeRef {
    /// The node's kind.
    pub fn kind(&self) -> NodeKind {
        match self {
            NodeRef::Symbol(_) => NodeKind::Symbol,
            NodeRef::File { .. } => NodeKind::File,
            NodeRef::Project(_) => NodeKind::Project,
            NodeRef::Contract { .. } => NodeKind::Contract,
            NodeRef::Name { .. } => NodeKind::Name,
        }
    }

    /// Storage triple (kind, id, key).
    pub(crate) fn parts(&self) -> Result<(NodeKind, Uuid, String), StoreError> {
        Ok(match self {
            NodeRef::Symbol(id) => (NodeKind::Symbol, id.0, String::new()),
            NodeRef::File { project, path } => (NodeKind::File, project.0, path.to_string()),
            NodeRef::Project(id) => (NodeKind::Project, id.0, String::new()),
            NodeRef::Contract {
                workspace,
                kind,
                key,
            } => {
                if key.is_empty() {
                    return Err(StoreError::invalid("contract key must not be empty"));
                }
                (NodeKind::Contract, workspace.0, format!("{kind}:{key}"))
            }
            NodeRef::Name { project, name } => {
                if name.is_empty() {
                    return Err(StoreError::invalid("node name must not be empty"));
                }
                (NodeKind::Name, project.0, name.clone())
            }
        })
    }

    /// Rebuilds a node from its storage triple.
    pub(crate) fn from_parts(kind: NodeKind, id: Uuid, key: String) -> Result<Self, StoreError> {
        Ok(match kind {
            NodeKind::Symbol => NodeRef::Symbol(SymbolId(id)),
            NodeKind::Project => NodeRef::Project(ProjectId(id)),
            NodeKind::File => NodeRef::File {
                project: ProjectId(id),
                path: stored_path(key)?,
            },
            NodeKind::Contract => {
                let (kind, key) = key
                    .split_once(':')
                    .ok_or_else(|| StoreError::Corrupt("stored contract node key".to_owned()))?;
                NodeRef::Contract {
                    workspace: WorkspaceId(id),
                    kind: kind
                        .parse()
                        .map_err(|_| StoreError::Corrupt("stored contract node kind".to_owned()))?,
                    key: key.to_owned(),
                }
            }
            NodeKind::Name => NodeRef::Name {
                project: ProjectId(id),
                name: key,
            },
        })
    }
}

/// An edge to record in a building generation.
#[derive(Debug, Clone, PartialEq)]
pub struct NewEdge {
    /// Source node.
    pub from: NodeRef,
    /// Target node.
    pub to: NodeRef,
    /// Relation (`calls`, `imports`, `implements`, `tests`, `produces`, ...).
    pub kind: String,
    /// How the edge was established.
    pub evidence_type: EvidenceType,
    /// Whether the target is known.
    pub resolution: Resolution,
    /// Evidence references (file, range, rule, tool, ...), as JSON.
    pub evidence: serde_json::Value,
    /// Groups the edges one analysis step produced; by convention the
    /// repository path of the analysed file. Replacement works per origin.
    pub origin: String,
}

/// A stored edge.
#[derive(Debug, Clone, PartialEq)]
pub struct Edge {
    /// Id.
    pub id: EdgeId,
    /// View whose analysis produced it.
    pub view: ViewId,
    /// The edge's fields.
    pub edge: NewEdge,
    /// First generation that has it.
    pub valid_from: i64,
    /// First generation that no longer has it (`None` = current).
    pub valid_to: Option<i64>,
}

/// Direction of a traversal.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WalkDirection {
    /// Follow edges from source to target ("what does this use?").
    Outgoing,
    /// Follow edges from target to source ("what uses this?").
    Incoming,
    /// Follow edges both ways (e.g. producer -> contract <- consumer).
    Both,
}

/// Bounds and filters of a traversal.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WalkOptions {
    /// Maximum number of hops, 1..=[`MAX_WALK_DEPTH`].
    pub max_depth: u32,
    /// Direction.
    pub direction: WalkDirection,
    /// Edge kinds to follow; empty follows every kind.
    pub kinds: Vec<String>,
    /// Maximum number of distinct edges returned, 1..=[`MAX_WALK_EDGES`].
    pub max_edges: u32,
}

impl Default for WalkOptions {
    fn default() -> Self {
        Self {
            max_depth: 3,
            direction: WalkDirection::Outgoing,
            kinds: Vec::new(),
            max_edges: 500,
        }
    }
}

/// One edge reached by a traversal.
#[derive(Debug, Clone, PartialEq)]
pub struct WalkStep {
    /// Hops from the start node (1 = adjacent).
    pub depth: u32,
    /// The edge.
    pub edge: Edge,
    /// The node this edge led to.
    pub reached: NodeRef,
}

/// Result of [`walk_edges`].
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Walk {
    /// Distinct edges, each at its smallest depth, ordered by depth then id.
    pub steps: Vec<WalkStep>,
    /// Whether the edge budget cut the traversal short.
    pub truncated: bool,
}

/// Replaces the edges of `origins` in a building generation: their current
/// edges are closed at `generation` and `edges` start there. Every edge's
/// origin must be one of `origins`. Repeating the call for the same origins in
/// the same generation replaces the earlier attempt. Returns edges written.
pub async fn replace_edges(
    conn: &mut PgConnection,
    view: ViewId,
    generation: i64,
    origins: &[String],
    edges: &[NewEdge],
) -> Result<u64, StoreError> {
    let scope = origin_scope(origins, edges.iter().map(|e| e.origin.as_str()))?;
    for e in edges {
        if e.kind.is_empty() {
            return Err(StoreError::invalid("edge kind must not be empty"));
        }
    }
    let mut tx = conn.begin().await?;
    lock_building(&mut tx, view, generation).await?;
    replace_scope(&mut tx, Scoped::Edge, view, generation, &scope).await?;
    let mut written = 0;
    for batch in edges.chunks(BATCH_ROWS) {
        let n = batch.len();
        let (mut o, mut fk, mut fi, mut fy) = (
            Vec::with_capacity(n),
            Vec::with_capacity(n),
            Vec::with_capacity(n),
            Vec::with_capacity(n),
        );
        let (mut tk, mut ti, mut ty) = (
            Vec::with_capacity(n),
            Vec::with_capacity(n),
            Vec::with_capacity(n),
        );
        let (mut kinds, mut evidence_types, mut resolutions, mut evidence) = (
            Vec::with_capacity(n),
            Vec::with_capacity(n),
            Vec::with_capacity(n),
            Vec::with_capacity(n),
        );
        for e in batch {
            let (from_kind, from_id, from_key) = e.from.parts()?;
            let (to_kind, to_id, to_key) = e.to.parts()?;
            o.push(e.origin.as_str());
            fk.push(from_kind);
            fi.push(from_id);
            fy.push(from_key);
            tk.push(to_kind);
            ti.push(to_id);
            ty.push(to_key);
            kinds.push(e.kind.as_str());
            evidence_types.push(e.evidence_type);
            resolutions.push(e.resolution);
            evidence.push(e.evidence.clone());
        }
        written += sqlx::query(
            "INSERT INTO edge (view_id, valid_from, origin, from_kind, from_id, from_key,
                               to_kind, to_id, to_key, kind, evidence_type, resolution, evidence)
             SELECT $1, $2, u.* FROM unnest($3::text[], $4::node_kind[], $5::uuid[], $6::text[],
                                            $7::node_kind[], $8::uuid[], $9::text[], $10::text[],
                                            $11::evidence_type[], $12::edge_resolution[],
                                            $13::jsonb[]) AS u",
        )
        .bind(view)
        .bind(generation)
        .bind(&o)
        .bind(&fk)
        .bind(&fi)
        .bind(&fy)
        .bind(&tk)
        .bind(&ti)
        .bind(&ty)
        .bind(&kinds)
        .bind(&evidence_types)
        .bind(&resolutions)
        .bind(&evidence)
        .execute(&mut *tx)
        .await?
        .rows_affected();
    }
    tx.commit().await?;
    Ok(written)
}

/// Checks that every item origin is in `origins` and returns the sorted set.
fn origin_scope<'a>(
    origins: &'a [String],
    items: impl Iterator<Item = &'a str>,
) -> Result<Vec<&'a str>, StoreError> {
    let scope: BTreeSet<&str> = origins.iter().map(String::as_str).collect();
    if scope.contains("") {
        return Err(StoreError::invalid("origin must not be empty"));
    }
    for origin in items {
        if !scope.contains(origin) {
            return Err(StoreError::invalid(format!(
                "origin `{origin}` is outside the replaced origins"
            )));
        }
    }
    Ok(scope.into_iter().collect())
}

#[derive(sqlx::FromRow)]
struct EdgeRow {
    id: EdgeId,
    view_id: ViewId,
    origin: String,
    from_kind: NodeKind,
    from_id: Uuid,
    from_key: String,
    to_kind: NodeKind,
    to_id: Uuid,
    to_key: String,
    kind: String,
    evidence_type: EvidenceType,
    resolution: Resolution,
    evidence: serde_json::Value,
    valid_from: i64,
    valid_to: Option<i64>,
}

impl TryFrom<EdgeRow> for Edge {
    type Error = StoreError;

    fn try_from(row: EdgeRow) -> Result<Self, StoreError> {
        Ok(Edge {
            id: row.id,
            view: row.view_id,
            edge: NewEdge {
                from: NodeRef::from_parts(row.from_kind, row.from_id, row.from_key)?,
                to: NodeRef::from_parts(row.to_kind, row.to_id, row.to_key)?,
                kind: row.kind,
                evidence_type: row.evidence_type,
                resolution: row.resolution,
                evidence: row.evidence,
                origin: row.origin,
            },
            valid_from: row.valid_from,
            valid_to: row.valid_to,
        })
    }
}

macro_rules! walk_sql {
    ($start:literal, $next_kind:literal, $next_id:literal, $next_key:literal, $step:literal) => {
        concat!(
            "WITH RECURSIVE pins AS MATERIALIZED (
               SELECT * FROM unnest($4::uuid[], $5::bigint[]) AS p(view_id, generation)
             ),
             walk(edge_id, depth, node_kind, node_id, node_key, visited) AS (
               SELECT e.id, 1, ",
            $next_kind,
            ", ",
            $next_id,
            ", ",
            $next_key,
            ",
                      ARRAY[$1::text || ':' || $2::text || ':' || $3,
                            (",
            $next_kind,
            ")::text || ':' || (",
            $next_id,
            ")::text || ':' || ",
            $next_key,
            "]
               FROM edge e
               JOIN pins p ON p.view_id = e.view_id AND e.valid_from <= p.generation
                          AND (e.valid_to IS NULL OR e.valid_to > p.generation)
               WHERE (",
            $start,
            ")
                 AND (cardinality($6::text[]) = 0 OR e.kind = ANY($6))
               UNION ALL
               SELECT e.id, w.depth + 1, ",
            $next_kind,
            ", ",
            $next_id,
            ", ",
            $next_key,
            ",
                      w.visited || ((",
            $next_kind,
            ")::text || ':' || (",
            $next_id,
            ")::text || ':' || ",
            $next_key,
            ")
               FROM walk w
               JOIN edge e ON (",
            $step,
            ")
               JOIN pins p ON p.view_id = e.view_id AND e.valid_from <= p.generation
                          AND (e.valid_to IS NULL OR e.valid_to > p.generation)
               WHERE w.depth < $7
                 AND (cardinality($6::text[]) = 0 OR e.kind = ANY($6))
                 AND NOT (((",
            $next_kind,
            ")::text || ':' || (",
            $next_id,
            ")::text || ':' || ",
            $next_key,
            ") = ANY(w.visited))
             ),
             raw AS MATERIALIZED (SELECT * FROM walk LIMIT $8),
             firsts AS (
               SELECT DISTINCT ON (edge_id) edge_id, depth, node_kind, node_id, node_key
               FROM raw ORDER BY edge_id, depth
             )
             SELECT f.depth, f.node_kind AS reached_kind, f.node_id AS reached_id,
                    f.node_key AS reached_key, (SELECT count(*) FROM raw) AS raw_count,
                    e.id, e.view_id, e.origin, e.from_kind, e.from_id, e.from_key, e.to_kind,
                    e.to_id, e.to_key, e.kind, e.evidence_type, e.resolution, e.evidence,
                    e.valid_from, e.valid_to
             FROM firsts f JOIN edge e ON e.id = f.edge_id
             ORDER BY f.depth, e.id
             LIMIT $9"
        )
    };
}

const WALK_OUTGOING: &str = walk_sql!(
    "e.from_kind = $1 AND e.from_id = $2 AND e.from_key = $3",
    "e.to_kind",
    "e.to_id",
    "e.to_key",
    "e.from_kind = w.node_kind AND e.from_id = w.node_id AND e.from_key = w.node_key"
);

const WALK_INCOMING: &str = walk_sql!(
    "e.to_kind = $1 AND e.to_id = $2 AND e.to_key = $3",
    "e.from_kind",
    "e.from_id",
    "e.from_key",
    "e.to_kind = w.node_kind AND e.to_id = w.node_id AND e.to_key = w.node_key"
);

// For both directions the next node is whichever end is not the current one.
// The start term compares against the start node ($1..$3); the recursive term
// against the node reached so far (w.*), so the CASE expressions differ.
const WALK_BOTH: &str = "WITH RECURSIVE pins AS MATERIALIZED (
       SELECT * FROM unnest($4::uuid[], $5::bigint[]) AS p(view_id, generation)
     ),
     start AS (
       SELECT e.id AS edge_id,
              CASE WHEN e.from_kind = $1 AND e.from_id = $2 AND e.from_key = $3
                   THEN e.to_kind ELSE e.from_kind END AS node_kind,
              CASE WHEN e.from_kind = $1 AND e.from_id = $2 AND e.from_key = $3
                   THEN e.to_id ELSE e.from_id END AS node_id,
              CASE WHEN e.from_kind = $1 AND e.from_id = $2 AND e.from_key = $3
                   THEN e.to_key ELSE e.from_key END AS node_key
       FROM edge e
       JOIN pins p ON p.view_id = e.view_id AND e.valid_from <= p.generation
                  AND (e.valid_to IS NULL OR e.valid_to > p.generation)
       WHERE ((e.from_kind = $1 AND e.from_id = $2 AND e.from_key = $3)
           OR (e.to_kind = $1 AND e.to_id = $2 AND e.to_key = $3))
         AND (cardinality($6::text[]) = 0 OR e.kind = ANY($6))
     ),
     walk(edge_id, depth, node_kind, node_id, node_key, visited) AS (
       SELECT s.edge_id, 1, s.node_kind, s.node_id, s.node_key,
              ARRAY[$1::text || ':' || $2::text || ':' || $3,
                    s.node_kind::text || ':' || s.node_id::text || ':' || s.node_key]
       FROM start s
       UNION ALL
       SELECT n.edge_id, n.depth, n.node_kind, n.node_id, n.node_key,
              n.visited || (n.node_kind::text || ':' || n.node_id::text || ':' || n.node_key)
       FROM (
         SELECT e.id AS edge_id, w.depth + 1 AS depth, w.visited,
                CASE WHEN e.from_kind = w.node_kind AND e.from_id = w.node_id
                          AND e.from_key = w.node_key
                     THEN e.to_kind ELSE e.from_kind END AS node_kind,
                CASE WHEN e.from_kind = w.node_kind AND e.from_id = w.node_id
                          AND e.from_key = w.node_key
                     THEN e.to_id ELSE e.from_id END AS node_id,
                CASE WHEN e.from_kind = w.node_kind AND e.from_id = w.node_id
                          AND e.from_key = w.node_key
                     THEN e.to_key ELSE e.from_key END AS node_key
         FROM walk w
         JOIN edge e ON (e.from_kind = w.node_kind AND e.from_id = w.node_id
                         AND e.from_key = w.node_key)
                     OR (e.to_kind = w.node_kind AND e.to_id = w.node_id
                         AND e.to_key = w.node_key)
         JOIN pins p ON p.view_id = e.view_id AND e.valid_from <= p.generation
                    AND (e.valid_to IS NULL OR e.valid_to > p.generation)
         WHERE w.depth < $7
           AND (cardinality($6::text[]) = 0 OR e.kind = ANY($6))
       ) n
       WHERE NOT ((n.node_kind::text || ':' || n.node_id::text || ':' || n.node_key)
                  = ANY(n.visited))
     ),
     raw AS MATERIALIZED (SELECT * FROM walk LIMIT $8),
     firsts AS (
       SELECT DISTINCT ON (edge_id) edge_id, depth, node_kind, node_id, node_key
       FROM raw ORDER BY edge_id, depth
     )
     SELECT f.depth, f.node_kind AS reached_kind, f.node_id AS reached_id,
            f.node_key AS reached_key, (SELECT count(*) FROM raw) AS raw_count,
            e.id, e.view_id, e.origin, e.from_kind, e.from_id, e.from_key, e.to_kind,
            e.to_id, e.to_key, e.kind, e.evidence_type, e.resolution, e.evidence,
            e.valid_from, e.valid_to
     FROM firsts f JOIN edge e ON e.id = f.edge_id
     ORDER BY f.depth, e.id
     LIMIT $9";

/// Traverses the edges valid in the pinned view generations, starting at
/// `start`, with a bounded `WITH RECURSIVE` query: at most
/// `options.max_depth` hops, only `options.kinds` (all if empty), never
/// revisiting a node on the same path, and at most `options.max_edges`
/// distinct edges (each reported at its smallest depth). Pins may span
/// projects, so the walk crosses projects through shared nodes such as
/// contracts.
pub async fn walk_edges(
    conn: &mut PgConnection,
    start: &NodeRef,
    pins: &[GenerationPin],
    options: &WalkOptions,
) -> Result<Walk, StoreError> {
    if !(1..=MAX_WALK_DEPTH).contains(&options.max_depth) {
        return Err(StoreError::invalid(format!(
            "walk depth must be 1..={MAX_WALK_DEPTH}, got {}",
            options.max_depth
        )));
    }
    if !(1..=MAX_WALK_EDGES).contains(&options.max_edges) {
        return Err(StoreError::invalid(format!(
            "walk edge budget must be 1..={MAX_WALK_EDGES}, got {}",
            options.max_edges
        )));
    }
    let (kind, id, key) = start.parts()?;
    let (views, generations) = split_pins(pins);
    let max_edges = i64::from(options.max_edges);
    // Raw path rows may repeat an edge at several depths; allow some slack
    // before the budget counts as exhausted.
    let raw_limit = max_edges.saturating_mul(4);
    let sql = match options.direction {
        WalkDirection::Outgoing => WALK_OUTGOING,
        WalkDirection::Incoming => WALK_INCOMING,
        WalkDirection::Both => WALK_BOTH,
    };

    #[derive(sqlx::FromRow)]
    struct Row {
        depth: i32,
        reached_kind: NodeKind,
        reached_id: Uuid,
        reached_key: String,
        raw_count: i64,
        #[sqlx(flatten)]
        edge: EdgeRow,
    }
    let rows = sqlx::query_as::<_, Row>(sql)
        .bind(kind)
        .bind(id)
        .bind(key)
        .bind(&views)
        .bind(&generations)
        .bind(&options.kinds)
        .bind(i32::try_from(options.max_depth).unwrap_or(i32::MAX))
        .bind(raw_limit)
        .bind(max_edges.saturating_add(1))
        .fetch_all(conn)
        .await?;
    let mut truncated = rows.first().is_some_and(|r| r.raw_count >= raw_limit);
    let mut steps = Vec::with_capacity(rows.len());
    for row in rows {
        if steps.len() >= usize::try_from(options.max_edges).unwrap_or(usize::MAX) {
            truncated = true;
            break;
        }
        steps.push(WalkStep {
            depth: u32::try_from(row.depth)
                .map_err(|_| StoreError::Corrupt("negative walk depth".to_owned()))?,
            reached: NodeRef::from_parts(row.reached_kind, row.reached_id, row.reached_key)?,
            edge: row.edge.try_into()?,
        });
    }
    Ok(Walk { steps, truncated })
}

/// Edges valid in the pinned generations that leave or enter `node`
/// (one hop, both directions), ordered by id.
pub async fn edges_at(
    conn: &mut PgConnection,
    node: &NodeRef,
    pins: &[GenerationPin],
) -> Result<Vec<Edge>, StoreError> {
    let (kind, id, key) = node.parts()?;
    let (views, generations) = split_pins(pins);
    sqlx::query_as::<_, EdgeRow>(
        "SELECT e.id, e.view_id, e.origin, e.from_kind, e.from_id, e.from_key, e.to_kind,
                e.to_id, e.to_key, e.kind, e.evidence_type, e.resolution, e.evidence,
                e.valid_from, e.valid_to
         FROM edge e
         JOIN unnest($4::uuid[], $5::bigint[]) AS p(view_id, generation)
           ON p.view_id = e.view_id AND e.valid_from <= p.generation
          AND (e.valid_to IS NULL OR e.valid_to > p.generation)
         WHERE (e.from_kind = $1 AND e.from_id = $2 AND e.from_key = $3)
            OR (e.to_kind = $1 AND e.to_id = $2 AND e.to_key = $3)
         ORDER BY e.id",
    )
    .bind(kind)
    .bind(id)
    .bind(key)
    .bind(&views)
    .bind(&generations)
    .fetch_all(conn)
    .await?
    .into_iter()
    .map(TryInto::try_into)
    .collect()
}

const EDGE_COLUMNS: &str = "e.id, e.view_id, e.origin, e.from_kind, e.from_id, e.from_key, e.to_kind,
     e.to_id, e.to_key, e.kind, e.evidence_type, e.resolution, e.evidence, e.valid_from, e.valid_to";

/// Edges of relation `kind` valid at `pin` whose target is one of
/// `targets` ("who imports this file", "who references these symbols"),
/// ordered by id. Uses the target index, so it stays cheap for a handful of
/// targets in a large view.
pub async fn edges_into(
    conn: &mut PgConnection,
    pin: GenerationPin,
    kind: &str,
    targets: &[NodeRef],
) -> Result<Vec<Edge>, StoreError> {
    let mut out = Vec::new();
    for batch in targets.chunks(BATCH_ROWS) {
        let mut kinds = Vec::with_capacity(batch.len());
        let mut ids = Vec::with_capacity(batch.len());
        let mut keys = Vec::with_capacity(batch.len());
        for target in batch {
            let (k, id, key) = target.parts()?;
            kinds.push(k);
            ids.push(id);
            keys.push(key);
        }
        let rows = sqlx::query_as::<_, EdgeRow>(sqlx::AssertSqlSafe(format!(
            "SELECT {EDGE_COLUMNS}
             FROM unnest($4::node_kind[], $5::uuid[], $6::text[]) AS t(kind, id, key)
             JOIN edge e ON e.to_kind = t.kind AND e.to_id = t.id AND e.to_key = t.key
             WHERE e.view_id = $1 AND e.kind = $3
               AND e.valid_from <= $2 AND (e.valid_to IS NULL OR e.valid_to > $2)
             ORDER BY e.id"
        )))
        .bind(pin.view)
        .bind(pin.generation)
        .bind(kind)
        .bind(&kinds)
        .bind(&ids)
        .bind(&keys)
        .fetch_all(&mut *conn)
        .await?;
        for row in rows {
            out.push(row.try_into()?);
        }
    }
    out.sort_by_key(|e: &Edge| e.id);
    out.dedup_by_key(|e| e.id);
    Ok(out)
}

/// Edges of relation `kind` valid at `pin` whose target is an unresolved
/// name ([`NodeRef::Name`]) of `project` and whose name's last
/// `/`-separated segment is one of `tails` (the whole name when it has no
/// `/`), ordered by id. Lets an indexer find path-like references (import
/// specifiers such as `./util` or `include/strings.h`) that a newly added
/// file `util.ts` or `strings.h` may now resolve, without listing every
/// unresolved name of the view.
pub async fn edges_into_name_tails(
    conn: &mut PgConnection,
    pin: GenerationPin,
    kind: &str,
    project: ProjectId,
    tails: &[String],
) -> Result<Vec<Edge>, StoreError> {
    if tails.is_empty() {
        return Ok(Vec::new());
    }
    let rows = sqlx::query_as::<_, EdgeRow>(sqlx::AssertSqlSafe(format!(
        "SELECT {EDGE_COLUMNS}
         FROM edge e
         WHERE e.to_kind = 'name' AND e.to_id = $4 AND e.view_id = $1 AND e.kind = $3
           AND e.valid_from <= $2 AND (e.valid_to IS NULL OR e.valid_to > $2)
           AND regexp_replace(e.to_key, '^.*/', '') = ANY($5::text[])
         ORDER BY e.id"
    )))
    .bind(pin.view)
    .bind(pin.generation)
    .bind(kind)
    .bind(project)
    .bind(tails)
    .fetch_all(conn)
    .await?;
    rows.into_iter().map(TryInto::try_into).collect()
}

/// Edges valid at `pin` that belong to one of `origins`, ordered by origin
/// and id: what an analysis step wrote earlier, for comparing against a new
/// result before replacing it.
pub async fn edges_with_origins(
    conn: &mut PgConnection,
    pin: GenerationPin,
    origins: &[String],
) -> Result<Vec<Edge>, StoreError> {
    let mut out = Vec::new();
    for batch in origins.chunks(BATCH_ROWS) {
        let rows = sqlx::query_as::<_, EdgeRow>(sqlx::AssertSqlSafe(format!(
            "SELECT {EDGE_COLUMNS}
             FROM edge e
             WHERE e.view_id = $1 AND e.origin = ANY($3::text[])
               AND e.valid_from <= $2 AND (e.valid_to IS NULL OR e.valid_to > $2)
             ORDER BY e.origin COLLATE \"C\", e.id"
        )))
        .bind(pin.view)
        .bind(pin.generation)
        .bind(batch)
        .fetch_all(&mut *conn)
        .await?;
        for row in rows {
            out.push(row.try_into()?);
        }
    }
    Ok(out)
}

/// Contract participations valid at `pin` that belong to one of `origins`,
/// ordered by origin and id (see [`edges_with_origins`]).
pub async fn contracts_with_origins(
    conn: &mut PgConnection,
    pin: GenerationPin,
    origins: &[String],
) -> Result<Vec<ContractParty>, StoreError> {
    let mut out = Vec::new();
    for batch in origins.chunks(BATCH_ROWS) {
        let rows = sqlx::query_as::<_, ContractRow>(
            "SELECT c.id, v.project_id, c.view_id, c.origin, c.kind, c.key, c.role, c.symbol_id,
                    c.evidence_type, c.evidence, c.valid_from, c.valid_to
             FROM contract c
             JOIN view v ON v.id = c.view_id
             WHERE c.view_id = $1 AND c.origin = ANY($3::text[])
               AND c.valid_from <= $2 AND (c.valid_to IS NULL OR c.valid_to > $2)
             ORDER BY c.origin COLLATE \"C\", c.id",
        )
        .bind(pin.view)
        .bind(pin.generation)
        .bind(batch)
        .fetch_all(&mut *conn)
        .await?;
        out.extend(rows.into_iter().map(ContractParty::from));
    }
    Ok(out)
}

#[derive(sqlx::FromRow)]
struct ContractRow {
    id: ContractId,
    project_id: ProjectId,
    view_id: ViewId,
    origin: String,
    kind: ContractKind,
    key: String,
    role: ContractRole,
    symbol_id: Option<SymbolId>,
    evidence_type: EvidenceType,
    evidence: serde_json::Value,
    valid_from: i64,
    valid_to: Option<i64>,
}

impl From<ContractRow> for ContractParty {
    fn from(r: ContractRow) -> Self {
        ContractParty {
            id: r.id,
            project: r.project_id,
            view: r.view_id,
            contract: NewContract {
                kind: r.kind,
                key: r.key,
                role: r.role,
                origin: r.origin,
                symbol: r.symbol_id,
                evidence_type: r.evidence_type,
                evidence: r.evidence,
            },
            valid_from: r.valid_from,
            valid_to: r.valid_to,
        }
    }
}

/// A contract participation to record in a building generation.
#[derive(Debug, Clone, PartialEq)]
pub struct NewContract {
    /// Kind of contract.
    pub kind: ContractKind,
    /// Contract key (`GET /orders/{id}`, `orders.created`, `DATABASE_URL`, ...).
    /// Environment contracts carry the variable **name**, never its value.
    pub key: String,
    /// Producer or consumer.
    pub role: ContractRole,
    /// Groups rows of one analysis step (by convention the file path).
    pub origin: String,
    /// Symbol that produces or consumes it, if known.
    pub symbol: Option<SymbolId>,
    /// How the participation was established.
    pub evidence_type: EvidenceType,
    /// Evidence references, as JSON.
    pub evidence: serde_json::Value,
}

/// A stored contract participation.
#[derive(Debug, Clone, PartialEq)]
pub struct ContractParty {
    /// Id.
    pub id: ContractId,
    /// Project of the view.
    pub project: ProjectId,
    /// View whose analysis found it.
    pub view: ViewId,
    /// The participation's fields.
    pub contract: NewContract,
    /// First generation that has it.
    pub valid_from: i64,
    /// First generation that no longer has it (`None` = current).
    pub valid_to: Option<i64>,
}

/// Replaces the contract participations of `origins` in a building
/// generation (same semantics as [`replace_edges`]). Returns rows written.
pub async fn replace_contracts(
    conn: &mut PgConnection,
    view: ViewId,
    generation: i64,
    origins: &[String],
    contracts: &[NewContract],
) -> Result<u64, StoreError> {
    let scope = origin_scope(origins, contracts.iter().map(|c| c.origin.as_str()))?;
    if contracts.iter().any(|c| c.key.is_empty()) {
        return Err(StoreError::invalid("contract key must not be empty"));
    }
    let mut tx = conn.begin().await?;
    lock_building(&mut tx, view, generation).await?;
    replace_scope(&mut tx, Scoped::Contract, view, generation, &scope).await?;
    let mut written = 0;
    for batch in contracts.chunks(BATCH_ROWS) {
        let origins: Vec<&str> = batch.iter().map(|c| c.origin.as_str()).collect();
        let kinds: Vec<ContractKind> = batch.iter().map(|c| c.kind).collect();
        let keys: Vec<&str> = batch.iter().map(|c| c.key.as_str()).collect();
        let roles: Vec<ContractRole> = batch.iter().map(|c| c.role).collect();
        let symbols: Vec<Option<SymbolId>> = batch.iter().map(|c| c.symbol).collect();
        let evidence_types: Vec<EvidenceType> = batch.iter().map(|c| c.evidence_type).collect();
        let evidence: Vec<serde_json::Value> = batch.iter().map(|c| c.evidence.clone()).collect();
        written += sqlx::query(
            "INSERT INTO contract (view_id, valid_from, origin, kind, key, role, symbol_id,
                                   evidence_type, evidence)
             SELECT $1, $2, u.* FROM unnest($3::text[], $4::contract_kind[], $5::text[],
                                            $6::contract_role[], $7::uuid[],
                                            $8::evidence_type[], $9::jsonb[]) AS u",
        )
        .bind(view)
        .bind(generation)
        .bind(&origins)
        .bind(&kinds)
        .bind(&keys)
        .bind(&roles)
        .bind(&symbols)
        .bind(&evidence_types)
        .bind(&evidence)
        .execute(&mut *tx)
        .await
        .map_err(|e| map_write(e, "contract", "", "symbol", "referenced by a contract"))?
        .rows_affected();
    }
    tx.commit().await?;
    Ok(written)
}

/// Producers and consumers of a contract in the pinned view generations,
/// ordered by role (producers first), project, origin and id.
pub async fn contract_parties(
    conn: &mut PgConnection,
    pins: &[GenerationPin],
    kind: ContractKind,
    key: &str,
) -> Result<Vec<ContractParty>, StoreError> {
    let (views, generations) = split_pins(pins);
    #[derive(sqlx::FromRow)]
    struct Row {
        id: ContractId,
        project_id: ProjectId,
        view_id: ViewId,
        origin: String,
        kind: ContractKind,
        key: String,
        role: ContractRole,
        symbol_id: Option<SymbolId>,
        evidence_type: EvidenceType,
        evidence: serde_json::Value,
        valid_from: i64,
        valid_to: Option<i64>,
    }
    let rows = sqlx::query_as::<_, Row>(
        "SELECT c.id, v.project_id, c.view_id, c.origin, c.kind, c.key, c.role, c.symbol_id,
                c.evidence_type, c.evidence, c.valid_from, c.valid_to
         FROM contract c
         JOIN view v ON v.id = c.view_id
         JOIN unnest($1::uuid[], $2::bigint[]) AS p(view_id, generation)
           ON p.view_id = c.view_id AND c.valid_from <= p.generation
          AND (c.valid_to IS NULL OR c.valid_to > p.generation)
         WHERE c.kind = $3 AND c.key = $4
         ORDER BY c.role, v.project_id, c.origin COLLATE \"C\", c.id",
    )
    .bind(&views)
    .bind(&generations)
    .bind(kind)
    .bind(key)
    .fetch_all(conn)
    .await?;
    Ok(rows
        .into_iter()
        .map(|r| ContractParty {
            id: r.id,
            project: r.project_id,
            view: r.view_id,
            contract: NewContract {
                kind: r.kind,
                key: r.key,
                role: r.role,
                origin: r.origin,
                symbol: r.symbol_id,
                evidence_type: r.evidence_type,
                evidence: r.evidence,
            },
            valid_from: r.valid_from,
            valid_to: r.valid_to,
        })
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn node_refs_round_trip_through_parts() {
        let id = Uuid::nil();
        let nodes = [
            NodeRef::Symbol(SymbolId(id)),
            NodeRef::Project(ProjectId(id)),
            NodeRef::File {
                project: ProjectId(id),
                path: RepoPath::new("src/a.rs").unwrap(),
            },
            NodeRef::Contract {
                workspace: WorkspaceId(id),
                kind: ContractKind::Endpoint,
                key: "GET /orders/{id}:v2".to_owned(),
            },
            NodeRef::Name {
                project: ProjectId(id),
                name: "fetchOrders".to_owned(),
            },
        ];
        for node in nodes {
            let (kind, id, key) = node.parts().unwrap();
            assert_eq!(kind, node.kind());
            assert_eq!(NodeRef::from_parts(kind, id, key).unwrap(), node);
        }
    }

    #[test]
    fn empty_keys_are_rejected() {
        let node = NodeRef::Name {
            project: ProjectId(Uuid::nil()),
            name: String::new(),
        };
        assert!(node.parts().is_err());
        assert!(NodeRef::from_parts(NodeKind::Contract, Uuid::nil(), "nocolon".into()).is_err());
        assert!(NodeRef::from_parts(NodeKind::Contract, Uuid::nil(), "bad:key".into()).is_err());
    }

    #[test]
    fn origin_scope_checks_membership() {
        let origins = vec!["a.rs".to_owned(), "b.rs".to_owned()];
        assert_eq!(
            origin_scope(&origins, ["a.rs"].into_iter()).unwrap(),
            vec!["a.rs", "b.rs"]
        );
        assert!(origin_scope(&origins, ["c.rs"].into_iter()).is_err());
        assert!(origin_scope(&[String::new()], std::iter::empty()).is_err());
    }

    #[test]
    fn walk_sql_variants_differ() {
        assert_ne!(WALK_OUTGOING, WALK_INCOMING);
        assert!(WALK_OUTGOING.contains("e.from_kind = w.node_kind"));
        assert!(WALK_INCOMING.contains("e.to_kind = w.node_kind"));
        assert!(WALK_BOTH.contains("OR (e.to_kind = w.node_kind"));
    }
}
