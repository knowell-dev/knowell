//! Content blobs, chunks, generation-scoped file versions and the per-path
//! prepared embedding inputs of their chunks ([`replace_chunk_inputs`],
//! [`chunk_inputs_at`], [`locate_chunk_inputs`]).
//!
//! Bulk writes bind one array per column and insert with `UNNEST`, in
//! batches of [`BATCH_ROWS`] rows; all of them are idempotent.

use std::collections::{BTreeMap, BTreeSet};

use knowell_core::{ContentHash, LineRange, RepoPath};
use sqlx::{Connection, PgConnection};
use time::OffsetDateTime;

use crate::error::StoreError;
use crate::hierarchy::stored_path;
use crate::ids::{OrganizationId, ViewId};
use crate::types::{from_i32, from_i64, hash_bytes, hash_from_bytes, to_i32, to_i64};
use crate::views::{GenerationPin, lock_building};

/// Rows per bulk statement. Keeps bind payloads well below protocol limits
/// while amortising round trips.
pub const BATCH_ROWS: usize = 5_000;

/// A content blob to store. `redacted_text` must already be redacted; the
/// store never sees raw secrets.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewContent {
    /// BLAKE3 hash of the original content.
    pub hash: ContentHash,
    /// Size of the original content in bytes.
    pub size_bytes: u64,
    /// Detected language, if any (e.g. `rust`).
    pub language: Option<String>,
    /// Redacted text; `None` when the content is not kept as text.
    pub redacted_text: Option<String>,
}

/// A stored content blob.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Content {
    /// Tenant the blob belongs to.
    pub organization: OrganizationId,
    /// BLAKE3 hash of the original content.
    pub hash: ContentHash,
    /// Size of the original content in bytes.
    pub size_bytes: u64,
    /// Detected language.
    pub language: Option<String>,
    /// Redacted text.
    pub redacted_text: Option<String>,
    /// When it was first stored.
    pub created_at: OffsetDateTime,
}

/// A chunk to store: one meaningful unit of a blob as cut by one parser
/// version. Chunks are keyed by (content hash, parser version, ordinal).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewChunk {
    /// Blob the chunk was cut from (must be stored first).
    pub content_hash: ContentHash,
    /// Parser / chunker version that produced it.
    pub parser_version: String,
    /// Position within the blob's chunks, from 0.
    pub ordinal: u32,
    /// Lines covered (1-based, inclusive).
    pub lines: LineRange,
    /// First byte (0-based, inclusive).
    pub start_byte: u64,
    /// End byte (0-based, exclusive).
    pub end_byte: u64,
    /// Kind of unit (`function`, `class`, `section`, ...).
    pub kind: String,
    /// Enclosing symbol path, if any.
    pub symbol_path: Option<String>,
    /// Hash of the prepared embedding input (content plus context header).
    pub prepared_input_hash: ContentHash,
}

/// A stored chunk.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Chunk {
    /// Tenant.
    pub organization: OrganizationId,
    /// The chunk's fields.
    pub chunk: NewChunk,
}

/// One change to the file list of a view in a building generation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FileChange {
    /// The path now has this content. With `renamed_from`, the file moved
    /// here from that path (which is closed too) and history follows it.
    Upsert {
        /// Path in the view.
        path: RepoPath,
        /// Its content.
        content_hash: ContentHash,
        /// Previous path of a renamed or moved file.
        renamed_from: Option<RepoPath>,
    },
    /// The path no longer exists.
    Delete {
        /// Path in the view.
        path: RepoPath,
    },
}

/// A path's content during a range of generations of a view.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileVersion {
    /// View.
    pub view: ViewId,
    /// Path in the view.
    pub path: RepoPath,
    /// Content during the range.
    pub content_hash: ContentHash,
    /// Previous path, when this version started with a rename.
    pub renamed_from: Option<RepoPath>,
    /// First generation that has this version.
    pub valid_from: i64,
    /// First generation that no longer has it (`None` = still current).
    pub valid_to: Option<i64>,
}

/// What [`apply_file_changes`] did.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct FileChangeSummary {
    /// New versions written.
    pub added: u64,
    /// Previous versions closed.
    pub closed: u64,
}

/// Stores content blobs; blobs that already exist are left unchanged
/// (content-addressed, immutable). Returns how many were new.
pub async fn upsert_contents(
    conn: &mut PgConnection,
    organization: OrganizationId,
    contents: &[NewContent],
) -> Result<u64, StoreError> {
    let mut inserted = 0;
    for batch in contents.chunks(BATCH_ROWS) {
        let mut hashes = Vec::with_capacity(batch.len());
        let mut sizes = Vec::with_capacity(batch.len());
        let mut languages = Vec::with_capacity(batch.len());
        let mut texts = Vec::with_capacity(batch.len());
        for c in batch {
            hashes.push(hash_bytes(&c.hash));
            sizes.push(to_i64(c.size_bytes, "content size")?);
            languages.push(c.language.as_deref());
            texts.push(c.redacted_text.as_deref());
        }
        inserted += sqlx::query(
            "INSERT INTO content (organization_id, hash, size_bytes, language, redacted_text)
             SELECT $1, t.hash, t.size, t.language, t.body
             FROM unnest($2::bytea[], $3::bigint[], $4::text[], $5::text[])
                  AS t(hash, size, language, body)
             ON CONFLICT (organization_id, hash) DO NOTHING",
        )
        .bind(organization)
        .bind(&hashes)
        .bind(&sizes)
        .bind(&languages)
        .bind(&texts)
        .execute(&mut *conn)
        .await?
        .rows_affected();
    }
    Ok(inserted)
}

/// Looks a blob up.
pub async fn get_content(
    conn: &mut PgConnection,
    organization: OrganizationId,
    hash: &ContentHash,
) -> Result<Option<Content>, StoreError> {
    let row: Option<(i64, Option<String>, Option<String>, OffsetDateTime)> = sqlx::query_as(
        "SELECT size_bytes, language, redacted_text, created_at
         FROM content WHERE organization_id = $1 AND hash = $2",
    )
    .bind(organization)
    .bind(hash_bytes(hash))
    .fetch_optional(conn)
    .await?;
    row.map(|(size, language, redacted_text, created_at)| {
        Ok(Content {
            organization,
            hash: *hash,
            size_bytes: from_i64(size, "content size")?,
            language,
            redacted_text,
            created_at,
        })
    })
    .transpose()
}

/// The redacted text of each of `hashes` that is stored as text, by hash, in
/// batches of [`BATCH_ROWS`]: one round trip per batch instead of one per
/// blob. Hashes without a stored blob or without text are absent.
pub async fn redacted_texts(
    conn: &mut PgConnection,
    organization: OrganizationId,
    hashes: &[ContentHash],
) -> Result<BTreeMap<ContentHash, String>, StoreError> {
    let unique: Vec<Vec<u8>> = hashes
        .iter()
        .collect::<BTreeSet<_>>()
        .into_iter()
        .map(hash_bytes)
        .collect();
    let mut out = BTreeMap::new();
    for batch in unique.chunks(BATCH_ROWS) {
        let rows: Vec<(Vec<u8>, String)> = sqlx::query_as(
            "SELECT hash, redacted_text FROM content
             WHERE organization_id = $1 AND hash = ANY($2::bytea[]) AND redacted_text IS NOT NULL",
        )
        .bind(organization)
        .bind(batch)
        .fetch_all(&mut *conn)
        .await?;
        for (hash, text) in rows {
            out.insert(hash_from_bytes(&hash)?, text);
        }
    }
    Ok(out)
}

/// Which of `hashes` are not stored yet, in input order without duplicates;
/// lets the pipeline skip reading and redacting known blobs.
pub async fn missing_contents(
    conn: &mut PgConnection,
    organization: OrganizationId,
    hashes: &[ContentHash],
) -> Result<Vec<ContentHash>, StoreError> {
    let mut missing = Vec::new();
    for batch in hashes.chunks(BATCH_ROWS) {
        let bytes: Vec<Vec<u8>> = batch.iter().map(hash_bytes).collect();
        let rows: Vec<Vec<u8>> = sqlx::query_scalar(
            "SELECT t.hash FROM unnest($2::bytea[]) WITH ORDINALITY AS t(hash, i)
             WHERE NOT EXISTS (SELECT 1 FROM content c
                               WHERE c.organization_id = $1 AND c.hash = t.hash)
             ORDER BY t.i",
        )
        .bind(organization)
        .bind(&bytes)
        .fetch_all(&mut *conn)
        .await?;
        for row in rows {
            missing.push(hash_from_bytes(&row)?);
        }
    }
    let mut seen = BTreeSet::new();
    missing.retain(|h| seen.insert(*h));
    Ok(missing)
}

/// Stores chunks; existing chunks with the same key are updated in place
/// (a no-op when nothing changed). A key may appear only once per call.
/// Returns how many rows were inserted or changed.
pub async fn upsert_chunks(
    conn: &mut PgConnection,
    organization: OrganizationId,
    chunks: &[NewChunk],
) -> Result<u64, StoreError> {
    let mut keys = BTreeSet::new();
    for c in chunks {
        if !keys.insert((c.content_hash, c.parser_version.as_str(), c.ordinal)) {
            return Err(StoreError::invalid(format!(
                "chunk {} of {} ({}) appears twice",
                c.ordinal,
                c.content_hash.short(),
                c.parser_version
            )));
        }
        if c.end_byte < c.start_byte {
            return Err(StoreError::invalid(
                "chunk end byte is before its start byte",
            ));
        }
        if c.parser_version.is_empty() || c.kind.is_empty() {
            return Err(StoreError::invalid(
                "chunk parser version and kind must not be empty",
            ));
        }
    }
    let mut written = 0;
    for batch in chunks.chunks(BATCH_ROWS) {
        let mut hashes = Vec::with_capacity(batch.len());
        let mut versions = Vec::with_capacity(batch.len());
        let mut ordinals = Vec::with_capacity(batch.len());
        let mut start_lines = Vec::with_capacity(batch.len());
        let mut end_lines = Vec::with_capacity(batch.len());
        let mut start_bytes = Vec::with_capacity(batch.len());
        let mut end_bytes = Vec::with_capacity(batch.len());
        let mut kinds = Vec::with_capacity(batch.len());
        let mut symbol_paths = Vec::with_capacity(batch.len());
        let mut prepared = Vec::with_capacity(batch.len());
        for c in batch {
            hashes.push(hash_bytes(&c.content_hash));
            versions.push(c.parser_version.as_str());
            ordinals.push(to_i32(c.ordinal, "chunk ordinal")?);
            start_lines.push(to_i32(c.lines.start(), "line")?);
            end_lines.push(to_i32(c.lines.end(), "line")?);
            start_bytes.push(to_i64(c.start_byte, "byte offset")?);
            end_bytes.push(to_i64(c.end_byte, "byte offset")?);
            kinds.push(c.kind.as_str());
            symbol_paths.push(c.symbol_path.as_deref());
            prepared.push(hash_bytes(&c.prepared_input_hash));
        }
        written += sqlx::query(
            "INSERT INTO chunk AS c (organization_id, content_hash, parser_version, ordinal,
                                     start_line, end_line, start_byte, end_byte, kind,
                                     symbol_path, prepared_input_hash)
             SELECT $1, t.* FROM unnest($2::bytea[], $3::text[], $4::int[], $5::int[], $6::int[],
                                        $7::bigint[], $8::bigint[], $9::text[], $10::text[],
                                        $11::bytea[]) AS t
             ON CONFLICT (organization_id, content_hash, parser_version, ordinal) DO UPDATE
             SET start_line = EXCLUDED.start_line, end_line = EXCLUDED.end_line,
                 start_byte = EXCLUDED.start_byte, end_byte = EXCLUDED.end_byte,
                 kind = EXCLUDED.kind, symbol_path = EXCLUDED.symbol_path,
                 prepared_input_hash = EXCLUDED.prepared_input_hash
             WHERE (c.start_line, c.end_line, c.start_byte, c.end_byte, c.kind, c.symbol_path,
                    c.prepared_input_hash)
                   IS DISTINCT FROM
                   (EXCLUDED.start_line, EXCLUDED.end_line, EXCLUDED.start_byte,
                    EXCLUDED.end_byte, EXCLUDED.kind, EXCLUDED.symbol_path,
                    EXCLUDED.prepared_input_hash)",
        )
        .bind(organization)
        .bind(&hashes)
        .bind(&versions)
        .bind(&ordinals)
        .bind(&start_lines)
        .bind(&end_lines)
        .bind(&start_bytes)
        .bind(&end_bytes)
        .bind(&kinds)
        .bind(&symbol_paths)
        .bind(&prepared)
        .execute(&mut *conn)
        .await
        .map_err(|e| match crate::error::violation(&e) {
            Some(crate::error::Violation::ForeignKey(_)) => StoreError::invalid(
                "chunks refer to content that is not stored; store the content first",
            ),
            _ => StoreError::Database(e),
        })?
        .rows_affected();
    }
    Ok(written)
}

#[derive(sqlx::FromRow)]
struct ChunkRow {
    content_hash: Vec<u8>,
    parser_version: String,
    ordinal: i32,
    start_line: i32,
    end_line: i32,
    start_byte: i64,
    end_byte: i64,
    kind: String,
    symbol_path: Option<String>,
    prepared_input_hash: Vec<u8>,
}

impl ChunkRow {
    fn into_chunk(self) -> Result<NewChunk, StoreError> {
        let lines = LineRange::new(
            from_i32(self.start_line, "line")?,
            from_i32(self.end_line, "line")?,
        )
        .map_err(|e| StoreError::Corrupt(format!("stored chunk lines: {e}")))?;
        Ok(NewChunk {
            content_hash: hash_from_bytes(&self.content_hash)?,
            parser_version: self.parser_version,
            ordinal: from_i32(self.ordinal, "chunk ordinal")?,
            lines,
            start_byte: from_i64(self.start_byte, "byte offset")?,
            end_byte: from_i64(self.end_byte, "byte offset")?,
            kind: self.kind,
            symbol_path: self.symbol_path,
            prepared_input_hash: hash_from_bytes(&self.prepared_input_hash)?,
        })
    }
}

/// The chunks of one blob for one parser version, by ordinal.
pub async fn chunks_of(
    conn: &mut PgConnection,
    organization: OrganizationId,
    content_hash: &ContentHash,
    parser_version: &str,
) -> Result<Vec<Chunk>, StoreError> {
    let rows = sqlx::query_as::<_, ChunkRow>(
        "SELECT content_hash, parser_version, ordinal, start_line, end_line, start_byte,
                end_byte, kind, symbol_path, prepared_input_hash
         FROM chunk
         WHERE organization_id = $1 AND content_hash = $2 AND parser_version = $3
         ORDER BY ordinal",
    )
    .bind(organization)
    .bind(hash_bytes(content_hash))
    .bind(parser_version)
    .fetch_all(conn)
    .await?;
    rows.into_iter()
        .map(|r| {
            Ok(Chunk {
                organization,
                chunk: r.into_chunk()?,
            })
        })
        .collect()
}

/// A chunk located in a pinned view: which file at which generation holds it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChunkLocation {
    /// The pin the location was found in.
    pub pin: GenerationPin,
    /// Path of the file in the view.
    pub path: RepoPath,
    /// The chunk (its content hash is the file's content at that generation).
    pub chunk: NewChunk,
}

/// Locates chunks by prepared input hash (e.g. vector search hits) in the
/// files of the pinned view generations. Pins of views outside
/// `organization` are ignored, so a lookup never crosses tenants. Ordered by
/// prepared input hash, view, path and ordinal.
pub async fn locate_prepared_inputs(
    conn: &mut PgConnection,
    organization: OrganizationId,
    pins: &[GenerationPin],
    prepared_input_hashes: &[ContentHash],
) -> Result<Vec<ChunkLocation>, StoreError> {
    let (views, generations) = split_pins(pins);
    let hashes: Vec<Vec<u8>> = prepared_input_hashes.iter().map(hash_bytes).collect();
    #[derive(sqlx::FromRow)]
    struct Row {
        view_id: ViewId,
        generation: i64,
        path: String,
        #[sqlx(flatten)]
        chunk: ChunkRow,
    }
    let rows = sqlx::query_as::<_, Row>(
        "WITH pins AS (
           SELECT p.view_id, p.generation
           FROM unnest($2::uuid[], $3::bigint[]) AS p(view_id, generation)
           JOIN view v ON v.id = p.view_id
           JOIN project pr ON pr.id = v.project_id AND pr.organization_id = $1
         )
         SELECT p.view_id, p.generation, f.path, c.content_hash, c.parser_version, c.ordinal,
                c.start_line, c.end_line, c.start_byte, c.end_byte, c.kind, c.symbol_path,
                c.prepared_input_hash
         FROM chunk c
         JOIN file_version f ON f.content_hash = c.content_hash
         JOIN pins p ON p.view_id = f.view_id
                    AND f.valid_from <= p.generation
                    AND (f.valid_to IS NULL OR f.valid_to > p.generation)
         WHERE c.organization_id = $1 AND c.prepared_input_hash = ANY($4::bytea[])
         ORDER BY c.prepared_input_hash, p.view_id, f.path COLLATE \"C\", c.parser_version COLLATE \"C\", c.ordinal",
    )
    .bind(organization)
    .bind(&views)
    .bind(&generations)
    .bind(&hashes)
    .fetch_all(conn)
    .await?;
    rows.into_iter()
        .map(|r| {
            Ok(ChunkLocation {
                pin: GenerationPin {
                    view: r.view_id,
                    generation: r.generation,
                },
                path: stored_path(r.path)?,
                chunk: r.chunk.into_chunk()?,
            })
        })
        .collect()
}

pub(crate) fn split_pins(pins: &[GenerationPin]) -> (Vec<ViewId>, Vec<i64>) {
    pins.iter().map(|p| (p.view, p.generation)).unzip()
}

/// The prepared embedding input of one chunk of a file, to record with
/// [`replace_chunk_inputs`].
///
/// The prepared input includes project and path context, so identical
/// content at two paths (or a file after a rename) has different inputs:
/// they are recorded per file version, while the content-level [`NewChunk`]
/// rows keep lines, bytes and kind for every path holding the content.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewChunkInput {
    /// Path of the file in the view.
    pub path: RepoPath,
    /// Content the chunk was cut from; must be the file's content at the
    /// pinned generation.
    pub content_hash: ContentHash,
    /// Position within the content's chunks (the [`NewChunk::ordinal`]).
    pub ordinal: u32,
    /// Hash of the prepared embedding input of this chunk at this path.
    pub prepared_input_hash: ContentHash,
    /// Whether the content policy lets the chunk be embedded.
    pub embed: bool,
}

/// A recorded per-path chunk input.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChunkInput {
    /// View.
    pub view: ViewId,
    /// Path of the file in the view.
    pub path: RepoPath,
    /// First generation of the file version the input belongs to.
    pub file_valid_from: i64,
    /// Content of that file version.
    pub content_hash: ContentHash,
    /// Parser / chunker version.
    pub parser_version: String,
    /// Chunk ordinal within the content.
    pub ordinal: u32,
    /// Hash of the prepared embedding input.
    pub prepared_input_hash: ContentHash,
    /// Whether the chunk is meant to be embedded.
    pub embed: bool,
}

/// Records the per-path chunk inputs of the file versions visible at `pin`:
/// for every path in `paths`, the inputs of `parser_version` are replaced by
/// the given ones (a path with no inputs ends up with none). Returns the
/// number of rows written.
///
/// Rows belong to the file version, not to the generation: they are not
/// fenced, may be written while the generation builds or after it was
/// activated (backfilling older data), and are removed together with their
/// file version (a failed generation's rollback, history pruning).
///
/// Fails with [`StoreError::InvalidInput`] when an input's path is not in
/// `paths`, an (path, ordinal) pair repeats, a path has no file version at
/// `pin`, or the file's content there is not the input's content hash.
pub async fn replace_chunk_inputs(
    conn: &mut PgConnection,
    pin: GenerationPin,
    parser_version: &str,
    paths: &[RepoPath],
    inputs: &[NewChunkInput],
) -> Result<u64, StoreError> {
    if parser_version.is_empty() {
        return Err(StoreError::invalid("parser version must not be empty"));
    }
    let scope: BTreeSet<&str> = paths.iter().map(RepoPath::as_str).collect();
    let mut keys = BTreeSet::new();
    for input in inputs {
        if !scope.contains(input.path.as_str()) {
            return Err(StoreError::invalid(format!(
                "chunk input of `{}` is outside the replaced paths",
                input.path
            )));
        }
        if !keys.insert((input.path.as_str(), input.ordinal)) {
            return Err(StoreError::invalid(format!(
                "chunk input {} of `{}` appears twice",
                input.ordinal, input.path
            )));
        }
    }
    if scope.is_empty() {
        return Ok(0);
    }
    let scope: Vec<&str> = scope.into_iter().collect();
    let mut tx = conn.begin().await?;
    let versions: Vec<(String, i64, Vec<u8>)> = sqlx::query_as(
        "SELECT path, valid_from, content_hash FROM file_version
         WHERE view_id = $1 AND path = ANY($3)
           AND valid_from <= $2 AND (valid_to IS NULL OR valid_to > $2)",
    )
    .bind(pin.view)
    .bind(pin.generation)
    .bind(&scope)
    .fetch_all(&mut *tx)
    .await?;
    let mut by_path: BTreeMap<&str, (i64, ContentHash)> = BTreeMap::new();
    for (path, valid_from, hash) in &versions {
        by_path.insert(path.as_str(), (*valid_from, hash_from_bytes(hash)?));
    }
    if let Some(missing) = scope.iter().find(|p| !by_path.contains_key(**p)) {
        return Err(StoreError::invalid(format!(
            "`{missing}` has no file version at generation {} of view {}",
            pin.generation, pin.view
        )));
    }
    let mut del_paths = Vec::with_capacity(by_path.len());
    let mut del_from = Vec::with_capacity(by_path.len());
    for (path, (valid_from, _)) in &by_path {
        del_paths.push(*path);
        del_from.push(*valid_from);
    }
    sqlx::query(
        "DELETE FROM chunk_input c
         USING unnest($2::text[], $3::bigint[]) AS u(path, valid_from)
         WHERE c.view_id = $1 AND c.path = u.path AND c.file_valid_from = u.valid_from
           AND c.parser_version = $4",
    )
    .bind(pin.view)
    .bind(&del_paths)
    .bind(&del_from)
    .bind(parser_version)
    .execute(&mut *tx)
    .await?;
    let mut written = 0;
    for batch in inputs.chunks(BATCH_ROWS) {
        let mut paths = Vec::with_capacity(batch.len());
        let mut froms = Vec::with_capacity(batch.len());
        let mut ordinals = Vec::with_capacity(batch.len());
        let mut hashes = Vec::with_capacity(batch.len());
        let mut embeds = Vec::with_capacity(batch.len());
        for input in batch {
            let Some((valid_from, content)) = by_path.get(input.path.as_str()) else {
                continue;
            };
            if *content != input.content_hash {
                return Err(StoreError::invalid(format!(
                    "`{}` does not have content {} at generation {}",
                    input.path,
                    input.content_hash.short(),
                    pin.generation
                )));
            }
            paths.push(input.path.as_str());
            froms.push(*valid_from);
            ordinals.push(to_i32(input.ordinal, "chunk ordinal")?);
            hashes.push(hash_bytes(&input.prepared_input_hash));
            embeds.push(input.embed);
        }
        written += sqlx::query(
            "INSERT INTO chunk_input (view_id, path, file_valid_from, parser_version, ordinal,
                                      prepared_input_hash, embed)
             SELECT $1, u.path, u.valid_from, $2, u.ordinal, u.hash, u.embed
             FROM unnest($3::text[], $4::bigint[], $5::int[], $6::bytea[], $7::boolean[])
                  AS u(path, valid_from, ordinal, hash, embed)",
        )
        .bind(pin.view)
        .bind(parser_version)
        .bind(&paths)
        .bind(&froms)
        .bind(&ordinals)
        .bind(&hashes)
        .bind(&embeds)
        .execute(&mut *tx)
        .await?
        .rows_affected();
    }
    tx.commit().await?;
    Ok(written)
}

#[derive(sqlx::FromRow)]
struct ChunkInputRow {
    view_id: ViewId,
    path: String,
    file_valid_from: i64,
    content_hash: Vec<u8>,
    parser_version: String,
    ordinal: i32,
    prepared_input_hash: Vec<u8>,
    embed: bool,
}

impl TryFrom<ChunkInputRow> for ChunkInput {
    type Error = StoreError;

    fn try_from(row: ChunkInputRow) -> Result<Self, StoreError> {
        Ok(Self {
            view: row.view_id,
            path: stored_path(row.path)?,
            file_valid_from: row.file_valid_from,
            content_hash: hash_from_bytes(&row.content_hash)?,
            parser_version: row.parser_version,
            ordinal: from_i32(row.ordinal, "chunk ordinal")?,
            prepared_input_hash: hash_from_bytes(&row.prepared_input_hash)?,
            embed: row.embed,
        })
    }
}

/// The per-path chunk inputs of `parser_version` of the file versions
/// visible at `pin` — all files, or only `paths` — ordered by path and
/// ordinal. A file without rows has no recorded inputs (no chunks, or data
/// indexed before per-path inputs existed).
pub async fn chunk_inputs_at(
    conn: &mut PgConnection,
    pin: GenerationPin,
    parser_version: &str,
    paths: Option<&[RepoPath]>,
) -> Result<Vec<ChunkInput>, StoreError> {
    let only: Option<Vec<&str>> = paths.map(|ps| ps.iter().map(RepoPath::as_str).collect());
    sqlx::query_as::<_, ChunkInputRow>(
        "SELECT c.view_id, c.path, c.file_valid_from, f.content_hash, c.parser_version,
                c.ordinal, c.prepared_input_hash, c.embed
         FROM file_version f
         JOIN chunk_input c ON c.view_id = f.view_id AND c.path = f.path
                           AND c.file_valid_from = f.valid_from
         WHERE f.view_id = $1 AND f.valid_from <= $2 AND (f.valid_to IS NULL OR f.valid_to > $2)
           AND c.parser_version = $3
           AND ($4::text[] IS NULL OR f.path = ANY($4))
         ORDER BY c.path COLLATE \"C\", c.ordinal",
    )
    .bind(pin.view)
    .bind(pin.generation)
    .bind(parser_version)
    .bind(only)
    .fetch_all(conn)
    .await?
    .into_iter()
    .map(TryInto::try_into)
    .collect()
}

/// Locates prepared inputs (for example vector search hits) in the files of
/// the pinned view generations, per path: each location is a file whose
/// recorded input (see [`replace_chunk_inputs`]) has the hash, with the
/// content-level chunk (lines, bytes, kind, symbol path) of that content.
/// In the returned [`ChunkLocation::chunk`], `prepared_input_hash` is the
/// per-path input that was looked up, so hits group by the hash the vector
/// was stored under.
///
/// For file versions that have no recorded inputs at all (data indexed
/// before per-path inputs existed) the content-level
/// `chunk.prepared_input_hash` is matched instead, as
/// [`locate_prepared_inputs`] does. Pins of views outside `organization`
/// are ignored, so a lookup never crosses tenants. Ordered by prepared input
/// hash, view, path and ordinal.
pub async fn locate_chunk_inputs(
    conn: &mut PgConnection,
    organization: OrganizationId,
    pins: &[GenerationPin],
    prepared_input_hashes: &[ContentHash],
) -> Result<Vec<ChunkLocation>, StoreError> {
    let (views, generations) = split_pins(pins);
    let hashes: Vec<Vec<u8>> = prepared_input_hashes.iter().map(hash_bytes).collect();
    #[derive(sqlx::FromRow)]
    struct Row {
        view_id: ViewId,
        generation: i64,
        path: String,
        #[sqlx(flatten)]
        chunk: ChunkRow,
    }
    let rows = sqlx::query_as::<_, Row>(
        "WITH pins AS (
           SELECT p.view_id, p.generation
           FROM unnest($2::uuid[], $3::bigint[]) AS p(view_id, generation)
           JOIN view v ON v.id = p.view_id
           JOIN project pr ON pr.id = v.project_id AND pr.organization_id = $1
         ),
         hits AS (
           SELECT p.view_id, p.generation, f.path, f.content_hash, ci.parser_version, ci.ordinal,
                  ci.prepared_input_hash
           FROM chunk_input ci
           JOIN file_version f ON f.view_id = ci.view_id AND f.path = ci.path
                              AND f.valid_from = ci.file_valid_from
           JOIN pins p ON p.view_id = f.view_id AND f.valid_from <= p.generation
                      AND (f.valid_to IS NULL OR f.valid_to > p.generation)
           WHERE ci.prepared_input_hash = ANY($4::bytea[])
           UNION ALL
           SELECT p.view_id, p.generation, f.path, f.content_hash, c.parser_version, c.ordinal,
                  c.prepared_input_hash
           FROM chunk c
           JOIN file_version f ON f.content_hash = c.content_hash
           JOIN pins p ON p.view_id = f.view_id AND f.valid_from <= p.generation
                      AND (f.valid_to IS NULL OR f.valid_to > p.generation)
           WHERE c.organization_id = $1 AND c.prepared_input_hash = ANY($4::bytea[])
             AND NOT EXISTS (SELECT 1 FROM chunk_input x
                             WHERE x.view_id = f.view_id AND x.path = f.path
                               AND x.file_valid_from = f.valid_from)
         )
         SELECT h.view_id, h.generation, h.path, c.content_hash, c.parser_version, c.ordinal,
                c.start_line, c.end_line, c.start_byte, c.end_byte, c.kind, c.symbol_path,
                h.prepared_input_hash
         FROM hits h
         JOIN chunk c ON c.organization_id = $1 AND c.content_hash = h.content_hash
                     AND c.parser_version = h.parser_version AND c.ordinal = h.ordinal
         ORDER BY h.prepared_input_hash, h.view_id, h.path COLLATE \"C\",
                  c.parser_version COLLATE \"C\", c.ordinal",
    )
    .bind(organization)
    .bind(&views)
    .bind(&generations)
    .bind(&hashes)
    .fetch_all(conn)
    .await?;
    rows.into_iter()
        .map(|r| {
            Ok(ChunkLocation {
                pin: GenerationPin {
                    view: r.view_id,
                    generation: r.generation,
                },
                path: stored_path(r.path)?,
                chunk: r.chunk.into_chunk()?,
            })
        })
        .collect()
}

/// Applies file changes to a building generation of `view`.
///
/// Changed paths get a new version starting at `generation`; their previous
/// version (and the old path of a rename) is closed at `generation`.
/// Unchanged content is left alone. Re-applying changes for a path within the
/// same generation replaces the earlier attempt, so retries are idempotent.
/// Fails with [`StoreError::GenerationNotBuilding`] once the generation was
/// activated or failed (the write fence).
pub async fn apply_file_changes(
    conn: &mut PgConnection,
    view: ViewId,
    generation: i64,
    changes: &[FileChange],
) -> Result<FileChangeSummary, StoreError> {
    let mut upserts: BTreeMap<&str, (&ContentHash, Option<&str>)> = BTreeMap::new();
    let mut deletes: BTreeSet<&str> = BTreeSet::new();
    for change in changes {
        let path = match change {
            FileChange::Upsert { path, .. } | FileChange::Delete { path } => path.as_str(),
        };
        if upserts.contains_key(path) || deletes.contains(path) {
            return Err(StoreError::invalid(format!(
                "path `{path}` appears more than once in one change set"
            )));
        }
        match change {
            FileChange::Upsert {
                path,
                content_hash,
                renamed_from,
            } => {
                if renamed_from.as_ref() == Some(path) {
                    return Err(StoreError::invalid(format!(
                        "path `{path}` cannot be renamed from itself"
                    )));
                }
                upserts.insert(
                    path.as_str(),
                    (content_hash, renamed_from.as_ref().map(RepoPath::as_str)),
                );
            }
            FileChange::Delete { path } => {
                deletes.insert(path.as_str());
            }
        }
    }
    // Paths whose open version is closed unconditionally.
    let mut closing: BTreeSet<&str> = deletes.clone();
    closing.extend(upserts.values().filter_map(|(_, from)| *from));
    let touched: Vec<&str> = closing
        .iter()
        .copied()
        .chain(upserts.keys().copied())
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect();
    let closing: Vec<&str> = closing.into_iter().collect();
    let up_paths: Vec<&str> = upserts.keys().copied().collect();
    let up_hashes: Vec<Vec<u8>> = upserts.values().map(|(h, _)| hash_bytes(h)).collect();
    let up_from: Vec<Option<&str>> = upserts.values().map(|(_, from)| *from).collect();

    let mut tx = conn.begin().await?;
    lock_building(&mut tx, view, generation).await?;
    // Undo an earlier attempt of this generation for these paths.
    sqlx::query(
        "DELETE FROM file_version WHERE view_id = $1 AND valid_from = $2 AND path = ANY($3)",
    )
    .bind(view)
    .bind(generation)
    .bind(&touched)
    .execute(&mut *tx)
    .await?;
    sqlx::query(
        "UPDATE file_version SET valid_to = NULL
         WHERE view_id = $1 AND valid_to = $2 AND path = ANY($3)",
    )
    .bind(view)
    .bind(generation)
    .bind(&touched)
    .execute(&mut *tx)
    .await?;
    let mut closed = sqlx::query(
        "UPDATE file_version SET valid_to = $2
         WHERE view_id = $1 AND valid_to IS NULL AND path = ANY($3)",
    )
    .bind(view)
    .bind(generation)
    .bind(&closing)
    .execute(&mut *tx)
    .await?
    .rows_affected();
    closed += sqlx::query(
        "UPDATE file_version f SET valid_to = $2
         FROM unnest($3::text[], $4::bytea[]) AS u(path, hash)
         WHERE f.view_id = $1 AND f.valid_to IS NULL AND f.path = u.path
           AND f.content_hash <> u.hash",
    )
    .bind(view)
    .bind(generation)
    .bind(&up_paths)
    .bind(&up_hashes)
    .execute(&mut *tx)
    .await?
    .rows_affected();
    // Paths whose open version already has this content stay as they are.
    let added = sqlx::query(
        "INSERT INTO file_version (view_id, path, valid_from, content_hash, renamed_from)
         SELECT $1, u.path, $2, u.hash, u.renamed_from
         FROM unnest($3::text[], $4::bytea[], $5::text[]) AS u(path, hash, renamed_from)
         WHERE NOT EXISTS (SELECT 1 FROM file_version f
                           WHERE f.view_id = $1 AND f.path = u.path AND f.valid_to IS NULL)",
    )
    .bind(view)
    .bind(generation)
    .bind(&up_paths)
    .bind(&up_hashes)
    .bind(&up_from)
    .execute(&mut *tx)
    .await?
    .rows_affected();
    tx.commit().await?;
    Ok(FileChangeSummary { added, closed })
}

#[derive(sqlx::FromRow)]
struct FileVersionRow {
    view_id: ViewId,
    path: String,
    content_hash: Vec<u8>,
    renamed_from: Option<String>,
    valid_from: i64,
    valid_to: Option<i64>,
}

impl TryFrom<FileVersionRow> for FileVersion {
    type Error = StoreError;

    fn try_from(row: FileVersionRow) -> Result<Self, StoreError> {
        Ok(Self {
            view: row.view_id,
            path: stored_path(row.path)?,
            content_hash: hash_from_bytes(&row.content_hash)?,
            renamed_from: row.renamed_from.map(stored_path).transpose()?,
            valid_from: row.valid_from,
            valid_to: row.valid_to,
        })
    }
}

/// Every file of a view at a generation, by path.
pub async fn files_at(
    conn: &mut PgConnection,
    pin: GenerationPin,
) -> Result<Vec<FileVersion>, StoreError> {
    sqlx::query_as::<_, FileVersionRow>(
        "SELECT view_id, path, content_hash, renamed_from, valid_from, valid_to
         FROM file_version
         WHERE view_id = $1 AND valid_from <= $2 AND (valid_to IS NULL OR valid_to > $2)
         ORDER BY path COLLATE \"C\"",
    )
    .bind(pin.view)
    .bind(pin.generation)
    .fetch_all(conn)
    .await?
    .into_iter()
    .map(TryInto::try_into)
    .collect()
}

/// One file of a view at a generation.
pub async fn file_at(
    conn: &mut PgConnection,
    pin: GenerationPin,
    path: &RepoPath,
) -> Result<Option<FileVersion>, StoreError> {
    let row = sqlx::query_as::<_, FileVersionRow>(
        "SELECT view_id, path, content_hash, renamed_from, valid_from, valid_to
         FROM file_version
         WHERE view_id = $1 AND path = $3
           AND valid_from <= $2 AND (valid_to IS NULL OR valid_to > $2)",
    )
    .bind(pin.view)
    .bind(pin.generation)
    .bind(path.as_str())
    .fetch_optional(conn)
    .await?;
    row.map(TryInto::try_into).transpose()
}

/// Versions of the file at `path`, newest first, following renames back to
/// earlier paths: each step is the version of the same (or the renamed-from)
/// path whose range ended exactly where the next one began. At most `limit`
/// versions.
pub async fn file_history(
    conn: &mut PgConnection,
    view: ViewId,
    path: &RepoPath,
    limit: u32,
) -> Result<Vec<FileVersion>, StoreError> {
    let limit = i32::try_from(limit.clamp(1, 10_000)).unwrap_or(10_000);
    sqlx::query_as::<_, FileVersionRow>(
        "WITH RECURSIVE history AS (
           SELECT * FROM (
             SELECT f.view_id, f.path, f.content_hash, f.renamed_from, f.valid_from, f.valid_to,
                    1 AS step
             FROM file_version f WHERE f.view_id = $1 AND f.path = $2
             ORDER BY f.valid_from DESC LIMIT 1
           ) newest
           UNION ALL
           SELECT p.view_id, p.path, p.content_hash, p.renamed_from, p.valid_from, p.valid_to,
                  h.step + 1
           FROM history h
           JOIN file_version p ON p.view_id = h.view_id
                              AND p.valid_to = h.valid_from
                              AND p.path = coalesce(h.renamed_from, h.path)
           WHERE h.step < $3
         )
         SELECT view_id, path, content_hash, renamed_from, valid_from, valid_to
         FROM history ORDER BY step",
    )
    .bind(view)
    .bind(path.as_str())
    .bind(limit)
    .fetch_all(conn)
    .await?
    .into_iter()
    .map(TryInto::try_into)
    .collect()
}
