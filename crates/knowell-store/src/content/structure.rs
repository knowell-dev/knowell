//! Optional source hierarchy of content-level chunks. No source bodies are read.

use std::collections::BTreeSet;

use knowell_core::{ContentHash, LineRange};
use sqlx::PgConnection;

use super::BATCH_ROWS;
use crate::StoreError;
use crate::ids::OrganizationId;
use crate::types::{from_i32, from_i64, hash_bytes, hash_from_bytes, to_i32, to_i64};

/// A contiguous range in a tenant's redacted source: inclusive 1-based lines
/// and half-open 0-based UTF-8 byte offsets. It is never embedding input text.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SourceRange {
    /// Source lines, including leading declaration comments and attributes.
    pub lines: LineRange,
    /// First source byte, inclusive.
    pub start_byte: u64,
    /// End source byte, exclusive.
    pub end_byte: u64,
}

/// Identity of one chunk within a tenant. The caller supplies the tenant
/// separately; path/view authorization remains the caller's responsibility.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct ChunkKey {
    /// Content hash of the source blob.
    pub content_hash: ContentHash,
    /// Parser/chunker version that produced the chunk.
    pub parser_version: String,
    /// Zero-based ordinal in that analysis.
    pub ordinal: u32,
}

/// Source hierarchy recorded alongside an existing chunk. Old indexes may
/// have no row; callers must not interpret missing metadata as `source_exact`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChunkStructure {
    /// Existing chunk whose source structure is described.
    pub key: ChunkKey,
    /// Earlier chunk ordinal: a container header or first continuation piece.
    pub parent_ordinal: Option<u32>,
    /// Whole declaration of the chunk's own symbol, when available.
    pub declaration: Option<SourceRange>,
    /// Whole declaration of that symbol's immediate enclosing symbol.
    pub enclosing: Option<SourceRange>,
    /// Whether embedding chunk text equals its exact contiguous source bytes.
    /// Elided container headers and joined top-level regions may be false.
    pub source_exact: bool,
}

fn validate(structure: &ChunkStructure) -> Result<(), StoreError> {
    if structure.key.parser_version.is_empty() {
        return Err(StoreError::invalid(
            "chunk structure parser version must not be empty",
        ));
    }
    if structure
        .parent_ordinal
        .is_some_and(|parent| parent >= structure.key.ordinal)
    {
        return Err(StoreError::invalid(
            "chunk structure parent must precede the chunk",
        ));
    }
    for range in [structure.declaration, structure.enclosing]
        .into_iter()
        .flatten()
    {
        if range.end_byte < range.start_byte {
            return Err(StoreError::invalid(
                "source range end byte is before its start byte",
            ));
        }
    }
    if let (Some(declaration), Some(enclosing)) = (structure.declaration, structure.enclosing)
        && (declaration.start_byte < enclosing.start_byte
            || declaration.end_byte > enclosing.end_byte
            || declaration.lines.start() < enclosing.lines.start()
            || declaration.lines.end() > enclosing.lines.end())
    {
        return Err(StoreError::invalid(
            "declaration range is outside its enclosing symbol",
        ));
    }
    Ok(())
}

/// Stores optional source hierarchy after its chunks have been stored. Keys
/// must occur once per call. Unchanged rows are not rewritten. Does not parse,
/// load source text, alter prepared inputs, or call embedding providers.
pub async fn upsert_chunk_structures(
    conn: &mut PgConnection,
    organization: OrganizationId,
    structures: &[ChunkStructure],
) -> Result<u64, StoreError> {
    let mut keys = BTreeSet::new();
    for structure in structures {
        validate(structure)?;
        if !keys.insert(&structure.key) {
            return Err(StoreError::invalid("chunk structure key appears twice"));
        }
    }
    let mut written = 0;
    for batch in structures.chunks(BATCH_ROWS) {
        let mut hashes = Vec::with_capacity(batch.len());
        let mut versions = Vec::with_capacity(batch.len());
        let mut ordinals = Vec::with_capacity(batch.len());
        let mut parents = Vec::with_capacity(batch.len());
        let mut declaration_start_lines = Vec::with_capacity(batch.len());
        let mut declaration_end_lines = Vec::with_capacity(batch.len());
        let mut declaration_start_bytes = Vec::with_capacity(batch.len());
        let mut declaration_end_bytes = Vec::with_capacity(batch.len());
        let mut enclosing_start_lines = Vec::with_capacity(batch.len());
        let mut enclosing_end_lines = Vec::with_capacity(batch.len());
        let mut enclosing_start_bytes = Vec::with_capacity(batch.len());
        let mut enclosing_end_bytes = Vec::with_capacity(batch.len());
        let mut exact = Vec::with_capacity(batch.len());
        for structure in batch {
            hashes.push(hash_bytes(&structure.key.content_hash));
            versions.push(structure.key.parser_version.as_str());
            ordinals.push(to_i32(structure.key.ordinal, "chunk ordinal")?);
            parents.push(
                structure
                    .parent_ordinal
                    .map(|value| to_i32(value, "parent ordinal"))
                    .transpose()?,
            );
            let declaration = range_columns(structure.declaration)?;
            declaration_start_lines.push(declaration.0);
            declaration_end_lines.push(declaration.1);
            declaration_start_bytes.push(declaration.2);
            declaration_end_bytes.push(declaration.3);
            let enclosing = range_columns(structure.enclosing)?;
            enclosing_start_lines.push(enclosing.0);
            enclosing_end_lines.push(enclosing.1);
            enclosing_start_bytes.push(enclosing.2);
            enclosing_end_bytes.push(enclosing.3);
            exact.push(structure.source_exact);
        }
        written += sqlx::query(
            "INSERT INTO chunk_structure AS c
               (organization_id, content_hash, parser_version, ordinal, parent_ordinal,
                declaration_start_line, declaration_end_line, declaration_start_byte,
                declaration_end_byte, enclosing_start_line, enclosing_end_line,
                enclosing_start_byte, enclosing_end_byte, source_exact)
             SELECT $1, t.* FROM unnest($2::bytea[], $3::text[], $4::int[], $5::int[],
                 $6::int[], $7::int[], $8::bigint[], $9::bigint[], $10::int[], $11::int[],
                 $12::bigint[], $13::bigint[], $14::boolean[]) AS t
             ON CONFLICT (organization_id, content_hash, parser_version, ordinal) DO UPDATE
             SET parent_ordinal = EXCLUDED.parent_ordinal,
                 declaration_start_line = EXCLUDED.declaration_start_line,
                 declaration_end_line = EXCLUDED.declaration_end_line,
                 declaration_start_byte = EXCLUDED.declaration_start_byte,
                 declaration_end_byte = EXCLUDED.declaration_end_byte,
                 enclosing_start_line = EXCLUDED.enclosing_start_line,
                 enclosing_end_line = EXCLUDED.enclosing_end_line,
                 enclosing_start_byte = EXCLUDED.enclosing_start_byte,
                 enclosing_end_byte = EXCLUDED.enclosing_end_byte,
                 source_exact = EXCLUDED.source_exact
             WHERE (c.parent_ordinal, c.declaration_start_line, c.declaration_end_line,
                    c.declaration_start_byte, c.declaration_end_byte, c.enclosing_start_line,
                    c.enclosing_end_line, c.enclosing_start_byte, c.enclosing_end_byte, c.source_exact)
                 IS DISTINCT FROM
                   (EXCLUDED.parent_ordinal, EXCLUDED.declaration_start_line,
                    EXCLUDED.declaration_end_line, EXCLUDED.declaration_start_byte,
                    EXCLUDED.declaration_end_byte, EXCLUDED.enclosing_start_line,
                    EXCLUDED.enclosing_end_line, EXCLUDED.enclosing_start_byte,
                    EXCLUDED.enclosing_end_byte, EXCLUDED.source_exact)",
        )
        .bind(organization)
        .bind(&hashes)
        .bind(&versions)
        .bind(&ordinals)
        .bind(&parents)
        .bind(&declaration_start_lines)
        .bind(&declaration_end_lines)
        .bind(&declaration_start_bytes)
        .bind(&declaration_end_bytes)
        .bind(&enclosing_start_lines)
        .bind(&enclosing_end_lines)
        .bind(&enclosing_start_bytes)
        .bind(&enclosing_end_bytes)
        .bind(&exact)
        .execute(&mut *conn)
        .await
        .map_err(|error| match crate::error::violation(&error) {
            Some(crate::error::Violation::ForeignKey(_)) => StoreError::invalid(
                "chunk structure refers to chunks that are not stored; store the chunks first",
            ),
            _ => StoreError::Database(error),
        })?
        .rows_affected();
    }
    Ok(written)
}

type RangeColumns = (Option<i32>, Option<i32>, Option<i64>, Option<i64>);

fn range_columns(range: Option<SourceRange>) -> Result<RangeColumns, StoreError> {
    match range {
        None => Ok((None, None, None, None)),
        Some(range) => Ok((
            Some(to_i32(range.lines.start(), "source line")?),
            Some(to_i32(range.lines.end(), "source line")?),
            Some(to_i64(range.start_byte, "source byte")?),
            Some(to_i64(range.end_byte, "source byte")?),
        )),
    }
}

fn source_range(columns: RangeColumns) -> Result<Option<SourceRange>, StoreError> {
    match columns {
        (None, None, None, None) => Ok(None),
        (Some(start), Some(end), Some(start_byte), Some(end_byte)) => {
            let lines = LineRange::new(
                from_i32(start, "source line")?,
                from_i32(end, "source line")?,
            )
            .map_err(|error| StoreError::Corrupt(format!("stored source lines: {error}")))?;
            let start_byte = from_i64(start_byte, "source byte")?;
            let end_byte = from_i64(end_byte, "source byte")?;
            if end_byte < start_byte {
                return Err(StoreError::Corrupt(
                    "stored source byte range is reversed".into(),
                ));
            }
            Ok(Some(SourceRange {
                lines,
                start_byte,
                end_byte,
            }))
        }
        _ => Err(StoreError::Corrupt(
            "stored source range is incomplete".into(),
        )),
    }
}

#[derive(sqlx::FromRow)]
struct StructureRow {
    content_hash: Vec<u8>,
    parser_version: String,
    ordinal: i32,
    parent_ordinal: Option<i32>,
    declaration_start_line: Option<i32>,
    declaration_end_line: Option<i32>,
    declaration_start_byte: Option<i64>,
    declaration_end_byte: Option<i64>,
    enclosing_start_line: Option<i32>,
    enclosing_end_line: Option<i32>,
    enclosing_start_byte: Option<i64>,
    enclosing_end_byte: Option<i64>,
    source_exact: bool,
}

impl StructureRow {
    fn into_structure(self) -> Result<ChunkStructure, StoreError> {
        let structure = ChunkStructure {
            key: ChunkKey {
                content_hash: hash_from_bytes(&self.content_hash)?,
                parser_version: self.parser_version,
                ordinal: from_i32(self.ordinal, "chunk ordinal")?,
            },
            parent_ordinal: self
                .parent_ordinal
                .map(|value| from_i32(value, "parent ordinal"))
                .transpose()?,
            declaration: source_range((
                self.declaration_start_line,
                self.declaration_end_line,
                self.declaration_start_byte,
                self.declaration_end_byte,
            ))?,
            enclosing: source_range((
                self.enclosing_start_line,
                self.enclosing_end_line,
                self.enclosing_start_byte,
                self.enclosing_end_byte,
            ))?,
            source_exact: self.source_exact,
        };
        validate(&structure)
            .map_err(|_| StoreError::Corrupt("stored chunk structure is invalid".into()))?;
        Ok(structure)
    }
}

/// Reads only the requested chunk keys, deduplicated and ordered by hash,
/// parser version and ordinal. Missing/legacy metadata is absent. The query
/// reads no source text and cannot cross the supplied tenant boundary.
pub async fn chunk_structures(
    conn: &mut PgConnection,
    organization: OrganizationId,
    keys: &[ChunkKey],
) -> Result<Vec<ChunkStructure>, StoreError> {
    let unique: Vec<&ChunkKey> = keys.iter().collect::<BTreeSet<_>>().into_iter().collect();
    let mut out = Vec::new();
    for batch in unique.chunks(BATCH_ROWS) {
        let hashes: Vec<Vec<u8>> = batch
            .iter()
            .map(|key| hash_bytes(&key.content_hash))
            .collect();
        let versions: Vec<&str> = batch
            .iter()
            .map(|key| key.parser_version.as_str())
            .collect();
        let ordinals = batch
            .iter()
            .map(|key| to_i32(key.ordinal, "chunk ordinal"))
            .collect::<Result<Vec<_>, _>>()?;
        let rows = sqlx::query_as::<_, StructureRow>(
            "SELECT c.* FROM chunk_structure c
             JOIN unnest($2::bytea[], $3::text[], $4::int[]) AS k(hash, version, ordinal)
               ON c.content_hash = k.hash AND c.parser_version = k.version AND c.ordinal = k.ordinal
             WHERE c.organization_id = $1
             ORDER BY c.content_hash, c.parser_version COLLATE \"C\", c.ordinal",
        )
        .bind(organization)
        .bind(&hashes)
        .bind(&versions)
        .bind(&ordinals)
        .fetch_all(&mut *conn)
        .await?;
        out.extend(
            rows.into_iter()
                .map(StructureRow::into_structure)
                .collect::<Result<Vec<_>, _>>()?,
        );
    }
    Ok(out)
}

/// Reads source hierarchy for the requested content hashes and parser version,
/// in hash/ordinal order. Missing legacy metadata remains absent. No source
/// bodies are loaded; callers should pass only authorized selected content.
pub async fn chunk_structures_of(
    conn: &mut PgConnection,
    organization: OrganizationId,
    hashes: &[ContentHash],
    parser_version: &str,
) -> Result<Vec<ChunkStructure>, StoreError> {
    let unique: Vec<Vec<u8>> = hashes
        .iter()
        .collect::<BTreeSet<_>>()
        .into_iter()
        .map(hash_bytes)
        .collect();
    let mut out = Vec::new();
    for batch in unique.chunks(BATCH_ROWS) {
        let rows = sqlx::query_as::<_, StructureRow>(
            "SELECT * FROM chunk_structure
             WHERE organization_id = $1 AND content_hash = ANY($2::bytea[]) AND parser_version = $3
             ORDER BY content_hash, ordinal",
        )
        .bind(organization)
        .bind(batch)
        .bind(parser_version)
        .fetch_all(&mut *conn)
        .await?;
        out.extend(
            rows.into_iter()
                .map(StructureRow::into_structure)
                .collect::<Result<Vec<_>, _>>()?,
        );
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn malformed_source_range_columns_are_rejected() {
        assert_eq!(source_range((None, None, None, None)).unwrap(), None);
        assert!(source_range((Some(1), None, Some(0), Some(4))).is_err());
        assert!(source_range((Some(0), Some(1), Some(0), Some(4))).is_err());
        assert!(source_range((Some(2), Some(1), Some(0), Some(4))).is_err());
        assert!(source_range((Some(1), Some(1), Some(4), Some(0))).is_err());
        assert!(source_range((Some(1), Some(1), Some(-1), Some(4))).is_err());
    }

    #[test]
    fn structures_reject_cycles_and_ranges_outside_the_parent() {
        let range = SourceRange {
            lines: LineRange::new(1, 3).unwrap(),
            start_byte: 0,
            end_byte: 20,
        };
        let mut structure = ChunkStructure {
            key: ChunkKey {
                content_hash: ContentHash::of(b"synthetic"),
                parser_version: "test".into(),
                ordinal: 1,
            },
            parent_ordinal: Some(0),
            declaration: Some(range),
            enclosing: Some(range),
            source_exact: true,
        };
        assert!(validate(&structure).is_ok());
        structure.parent_ordinal = Some(1);
        assert!(validate(&structure).is_err());
        structure.parent_ordinal = None;
        structure.declaration.as_mut().unwrap().end_byte = 21;
        assert!(validate(&structure).is_err());
    }
}
