//! Logical symbols and their generation-scoped occurrences.
//!
//! A symbol's id is its identity: it does not change when the code around it
//! changes, and [`rename_symbol`] keeps it across renames and moves.

use std::collections::BTreeSet;

use knowell_core::{ContentHash, LineRange, RepoPath};
use sqlx::{Connection, PgConnection};
use time::OffsetDateTime;

use crate::content::{BATCH_ROWS, split_pins};
use crate::error::{StoreError, map_write};
use crate::hierarchy::stored_path;
use crate::ids::{OccurrenceId, ProjectId, SymbolId, ViewId};
use crate::types::{OccurrenceRole, from_i32, hash_bytes, hash_from_bytes, to_i32};
use crate::views::{GenerationPin, lock_building};

/// A symbol to register.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewSymbol {
    /// Fully qualified name within the project (e.g. `billing::Service::cancel`).
    pub qualified_name: String,
    /// Kind (`function`, `method`, `class`, ...).
    pub kind: String,
}

/// A logical symbol.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Symbol {
    /// Stable id.
    pub id: SymbolId,
    /// Project.
    pub project: ProjectId,
    /// Current qualified name.
    pub qualified_name: String,
    /// Kind.
    pub kind: String,
    /// When it was first seen.
    pub created_at: OffsetDateTime,
    /// Last rename.
    pub updated_at: OffsetDateTime,
}

/// An occurrence to record in a building generation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewOccurrence {
    /// The symbol.
    pub symbol: SymbolId,
    /// File path in the view.
    pub path: RepoPath,
    /// Content of the file the lines refer to.
    pub content_hash: ContentHash,
    /// Lines (1-based, inclusive).
    pub lines: LineRange,
    /// Definition or reference.
    pub role: OccurrenceRole,
}

/// A stored occurrence.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Occurrence {
    /// Id.
    pub id: OccurrenceId,
    /// View.
    pub view: ViewId,
    /// The occurrence's fields.
    pub occurrence: NewOccurrence,
    /// First generation that has it.
    pub valid_from: i64,
    /// First generation that no longer has it (`None` = current).
    pub valid_to: Option<i64>,
}

#[derive(sqlx::FromRow)]
struct SymbolRow {
    id: SymbolId,
    project_id: ProjectId,
    qualified_name: String,
    kind: String,
    created_at: OffsetDateTime,
    updated_at: OffsetDateTime,
}

impl From<SymbolRow> for Symbol {
    fn from(row: SymbolRow) -> Self {
        Self {
            id: row.id,
            project: row.project_id,
            qualified_name: row.qualified_name,
            kind: row.kind,
            created_at: row.created_at,
            updated_at: row.updated_at,
        }
    }
}

/// Registers symbols (existing ones are kept) and returns their ids in input
/// order; duplicates in the input get the same id.
pub async fn upsert_symbols(
    conn: &mut PgConnection,
    project: ProjectId,
    symbols: &[NewSymbol],
) -> Result<Vec<SymbolId>, StoreError> {
    if symbols
        .iter()
        .any(|s| s.qualified_name.is_empty() || s.kind.is_empty())
    {
        return Err(StoreError::invalid(
            "symbol qualified name and kind must not be empty",
        ));
    }
    let mut ids = Vec::with_capacity(symbols.len());
    for batch in symbols.chunks(BATCH_ROWS) {
        let kinds: Vec<&str> = batch.iter().map(|s| s.kind.as_str()).collect();
        let names: Vec<&str> = batch.iter().map(|s| s.qualified_name.as_str()).collect();
        sqlx::query(
            "INSERT INTO symbol (project_id, kind, qualified_name)
             SELECT $1, u.kind, u.name FROM unnest($2::text[], $3::text[]) AS u(kind, name)
             ON CONFLICT (project_id, kind, qualified_name) DO NOTHING",
        )
        .bind(project)
        .bind(&kinds)
        .bind(&names)
        .execute(&mut *conn)
        .await
        .map_err(|e| map_write(e, "symbol", "", "project", project))?;
        let found: Vec<SymbolId> = sqlx::query_scalar(
            "SELECT s.id
             FROM unnest($2::text[], $3::text[]) WITH ORDINALITY AS u(kind, name, i)
             JOIN symbol s ON s.project_id = $1 AND s.kind = u.kind AND s.qualified_name = u.name
             ORDER BY u.i",
        )
        .bind(project)
        .bind(&kinds)
        .bind(&names)
        .fetch_all(&mut *conn)
        .await?;
        if found.len() != batch.len() {
            // A concurrent rename moved a symbol between insert and lookup.
            return Err(StoreError::Corrupt(
                "symbols changed while being registered; retry".to_owned(),
            ));
        }
        ids.extend(found);
    }
    Ok(ids)
}

/// Looks a symbol up by id.
pub async fn get_symbol(
    conn: &mut PgConnection,
    id: SymbolId,
) -> Result<Option<Symbol>, StoreError> {
    let row = sqlx::query_as::<_, SymbolRow>(
        "SELECT id, project_id, qualified_name, kind, created_at, updated_at
         FROM symbol WHERE id = $1",
    )
    .bind(id)
    .fetch_optional(conn)
    .await?;
    Ok(row.map(Into::into))
}

/// Symbols of a project with exactly this qualified name (one per kind), by kind.
pub async fn find_symbols(
    conn: &mut PgConnection,
    project: ProjectId,
    qualified_name: &str,
) -> Result<Vec<Symbol>, StoreError> {
    let rows = sqlx::query_as::<_, SymbolRow>(
        "SELECT id, project_id, qualified_name, kind, created_at, updated_at
         FROM symbol WHERE project_id = $1 AND qualified_name = $2 ORDER BY kind COLLATE \"C\"",
    )
    .bind(project)
    .bind(qualified_name)
    .fetch_all(conn)
    .await?;
    Ok(rows.into_iter().map(Into::into).collect())
}

/// Gives a symbol a new qualified name, keeping its id (and with it every
/// occurrence, edge and memory link).
pub async fn rename_symbol(
    conn: &mut PgConnection,
    id: SymbolId,
    qualified_name: &str,
) -> Result<Symbol, StoreError> {
    if qualified_name.is_empty() {
        return Err(StoreError::invalid(
            "symbol qualified name must not be empty",
        ));
    }
    let row = sqlx::query_as::<_, SymbolRow>(
        "UPDATE symbol SET qualified_name = $2, updated_at = now() WHERE id = $1
         RETURNING id, project_id, qualified_name, kind, created_at, updated_at",
    )
    .bind(id)
    .bind(qualified_name)
    .fetch_optional(conn)
    .await
    .map_err(|e| map_write(e, "symbol", qualified_name, "symbol", id))?;
    row.map(Into::into)
        .ok_or_else(|| StoreError::not_found("symbol", id))
}

/// Replaces the occurrences in `paths` for a building generation: their
/// current occurrences are closed at `generation` and `occurrences` start
/// there. Every occurrence must lie in one of `paths`. Repeating the call for
/// the same paths in the same generation replaces the earlier attempt.
/// Returns the number of occurrences written.
pub async fn replace_occurrences(
    conn: &mut PgConnection,
    view: ViewId,
    generation: i64,
    paths: &[RepoPath],
    occurrences: &[NewOccurrence],
) -> Result<u64, StoreError> {
    let scope: BTreeSet<&str> = paths.iter().map(RepoPath::as_str).collect();
    if let Some(o) = occurrences
        .iter()
        .find(|o| !scope.contains(o.path.as_str()))
    {
        return Err(StoreError::invalid(format!(
            "occurrence in `{}` is outside the replaced paths",
            o.path
        )));
    }
    let scope: Vec<&str> = scope.into_iter().collect();
    let mut tx = conn.begin().await?;
    lock_building(&mut tx, view, generation).await?;
    replace_scope(&mut tx, Scoped::Occurrence, view, generation, &scope).await?;
    let mut written = 0;
    for batch in occurrences.chunks(BATCH_ROWS) {
        let mut symbols = Vec::with_capacity(batch.len());
        let mut batch_paths = Vec::with_capacity(batch.len());
        let mut hashes = Vec::with_capacity(batch.len());
        let mut starts = Vec::with_capacity(batch.len());
        let mut ends = Vec::with_capacity(batch.len());
        let mut roles = Vec::with_capacity(batch.len());
        for o in batch {
            symbols.push(o.symbol);
            batch_paths.push(o.path.as_str());
            hashes.push(hash_bytes(&o.content_hash));
            starts.push(to_i32(o.lines.start(), "line")?);
            ends.push(to_i32(o.lines.end(), "line")?);
            roles.push(o.role);
        }
        written += sqlx::query(
            "INSERT INTO occurrence (symbol_id, view_id, path, content_hash, start_line, end_line,
                                     role, valid_from)
             SELECT u.symbol, $1, u.path, u.hash, u.start_line, u.end_line, u.role, $2
             FROM unnest($3::uuid[], $4::text[], $5::bytea[], $6::int[], $7::int[],
                         $8::occurrence_role[])
                  AS u(symbol, path, hash, start_line, end_line, role)",
        )
        .bind(view)
        .bind(generation)
        .bind(&symbols)
        .bind(&batch_paths)
        .bind(&hashes)
        .bind(&starts)
        .bind(&ends)
        .bind(&roles)
        .execute(&mut *tx)
        .await
        .map_err(|e| map_write(e, "occurrence", "", "symbol", "referenced by an occurrence"))?
        .rows_affected();
    }
    tx.commit().await?;
    Ok(written)
}

#[derive(sqlx::FromRow)]
struct OccurrenceRow {
    id: OccurrenceId,
    symbol_id: SymbolId,
    view_id: ViewId,
    path: String,
    content_hash: Vec<u8>,
    start_line: i32,
    end_line: i32,
    role: OccurrenceRole,
    valid_from: i64,
    valid_to: Option<i64>,
}

impl TryFrom<OccurrenceRow> for Occurrence {
    type Error = StoreError;

    fn try_from(row: OccurrenceRow) -> Result<Self, StoreError> {
        let lines = LineRange::new(
            from_i32(row.start_line, "line")?,
            from_i32(row.end_line, "line")?,
        )
        .map_err(|e| StoreError::Corrupt(format!("stored occurrence lines: {e}")))?;
        Ok(Self {
            id: row.id,
            view: row.view_id,
            occurrence: NewOccurrence {
                symbol: row.symbol_id,
                path: stored_path(row.path)?,
                content_hash: hash_from_bytes(&row.content_hash)?,
                lines,
                role: row.role,
            },
            valid_from: row.valid_from,
            valid_to: row.valid_to,
        })
    }
}

/// Occurrences of a symbol in the pinned view generations, ordered by view,
/// path and line.
pub async fn occurrences_of(
    conn: &mut PgConnection,
    symbol: SymbolId,
    pins: &[GenerationPin],
) -> Result<Vec<Occurrence>, StoreError> {
    let (views, generations) = split_pins(pins);
    sqlx::query_as::<_, OccurrenceRow>(
        "SELECT o.id, o.symbol_id, o.view_id, o.path, o.content_hash, o.start_line, o.end_line,
                o.role, o.valid_from, o.valid_to
         FROM occurrence o
         JOIN unnest($2::uuid[], $3::bigint[]) AS p(view_id, generation)
           ON p.view_id = o.view_id AND o.valid_from <= p.generation
          AND (o.valid_to IS NULL OR o.valid_to > p.generation)
         WHERE o.symbol_id = $1
         ORDER BY o.view_id, o.path COLLATE \"C\", o.start_line, o.end_line, o.role, o.id",
    )
    .bind(symbol)
    .bind(&views)
    .bind(&generations)
    .fetch_all(conn)
    .await?
    .into_iter()
    .map(TryInto::try_into)
    .collect()
}

/// Occurrences in one file of a view at a generation, by line.
pub async fn occurrences_in_file(
    conn: &mut PgConnection,
    pin: GenerationPin,
    path: &RepoPath,
) -> Result<Vec<Occurrence>, StoreError> {
    sqlx::query_as::<_, OccurrenceRow>(
        "SELECT id, symbol_id, view_id, path, content_hash, start_line, end_line, role,
                valid_from, valid_to
         FROM occurrence
         WHERE view_id = $1 AND path = $3
           AND valid_from <= $2 AND (valid_to IS NULL OR valid_to > $2)
         ORDER BY start_line, end_line, role, id",
    )
    .bind(pin.view)
    .bind(pin.generation)
    .bind(path.as_str())
    .fetch_all(conn)
    .await?
    .into_iter()
    .map(TryInto::try_into)
    .collect()
}

/// A symbol defined in a file of a pinned generation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Definition {
    /// The symbol, under its current name.
    pub symbol: Symbol,
    /// File holding the definition.
    pub path: RepoPath,
    /// Content of that file the lines refer to.
    pub content_hash: ContentHash,
    /// Lines of the definition (1-based, inclusive).
    pub lines: LineRange,
}

/// The definitions (occurrences with role `definition`) in the given files
/// of a view at a generation, with their symbols, ordered by path, line and
/// symbol id. Paths without definitions contribute nothing.
pub async fn definitions_in_paths(
    conn: &mut PgConnection,
    pin: GenerationPin,
    paths: &[RepoPath],
) -> Result<Vec<Definition>, StoreError> {
    let wanted: Vec<&str> = paths
        .iter()
        .map(RepoPath::as_str)
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect();
    #[derive(sqlx::FromRow)]
    struct Row {
        path: String,
        content_hash: Vec<u8>,
        start_line: i32,
        end_line: i32,
        #[sqlx(flatten)]
        symbol: SymbolRow,
    }
    let mut out = Vec::new();
    for batch in wanted.chunks(BATCH_ROWS) {
        let rows = sqlx::query_as::<_, Row>(
            "SELECT o.path, o.content_hash, o.start_line, o.end_line,
                    s.id, s.project_id, s.qualified_name, s.kind, s.created_at, s.updated_at
             FROM occurrence o
             JOIN symbol s ON s.id = o.symbol_id
             WHERE o.view_id = $1 AND o.path = ANY($3) AND o.role = 'definition'
               AND o.valid_from <= $2 AND (o.valid_to IS NULL OR o.valid_to > $2)
             ORDER BY o.path COLLATE \"C\", o.start_line, s.id",
        )
        .bind(pin.view)
        .bind(pin.generation)
        .bind(batch)
        .fetch_all(&mut *conn)
        .await?;
        for row in rows {
            let lines = LineRange::new(
                from_i32(row.start_line, "line")?,
                from_i32(row.end_line, "line")?,
            )
            .map_err(|e| StoreError::Corrupt(format!("stored occurrence lines: {e}")))?;
            out.push(Definition {
                symbol: row.symbol.into(),
                path: stored_path(row.path)?,
                content_hash: hash_from_bytes(&row.content_hash)?,
                lines,
            });
        }
    }
    Ok(out)
}

/// Generation-scoped tables whose rows are replaced per key (path or origin).
#[derive(Debug, Clone, Copy)]
pub(crate) enum Scoped {
    Occurrence,
    Edge,
    Contract,
}

/// Inside a fenced transaction: undoes an earlier attempt of `generation`
/// for `keys`, then closes the open rows for `keys` at `generation`.
pub(crate) async fn replace_scope(
    conn: &mut PgConnection,
    table: Scoped,
    view: ViewId,
    generation: i64,
    keys: &[&str],
) -> Result<(), StoreError> {
    let statements: [&'static str; 3] = match table {
        Scoped::Occurrence => [
            "DELETE FROM occurrence WHERE view_id = $1 AND valid_from = $2 AND path = ANY($3)",
            "UPDATE occurrence SET valid_to = NULL WHERE view_id = $1 AND valid_to = $2 AND path = ANY($3)",
            "UPDATE occurrence SET valid_to = $2 WHERE view_id = $1 AND valid_to IS NULL AND path = ANY($3)",
        ],
        Scoped::Edge => [
            "DELETE FROM edge WHERE view_id = $1 AND valid_from = $2 AND origin = ANY($3)",
            "UPDATE edge SET valid_to = NULL WHERE view_id = $1 AND valid_to = $2 AND origin = ANY($3)",
            "UPDATE edge SET valid_to = $2 WHERE view_id = $1 AND valid_to IS NULL AND origin = ANY($3)",
        ],
        Scoped::Contract => [
            "DELETE FROM contract WHERE view_id = $1 AND valid_from = $2 AND origin = ANY($3)",
            "UPDATE contract SET valid_to = NULL WHERE view_id = $1 AND valid_to = $2 AND origin = ANY($3)",
            "UPDATE contract SET valid_to = $2 WHERE view_id = $1 AND valid_to IS NULL AND origin = ANY($3)",
        ],
    };
    for sql in statements {
        sqlx::query(sql)
            .bind(view)
            .bind(generation)
            .bind(keys)
            .execute(&mut *conn)
            .await?;
    }
    Ok(())
}
