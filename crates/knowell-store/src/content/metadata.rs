//! Tenant-scoped generation metadata without source-body transfer.

use std::collections::BTreeSet;

use knowell_core::RepoPath;
use sqlx::PgConnection;

use super::{BATCH_ROWS, Content, FileVersion};
use crate::StoreError;
use crate::hierarchy::stored_path;
use crate::ids::{OrganizationId, ViewId};
use crate::types::{from_i64, hash_bytes, hash_from_bytes, to_i64, validate_commit};
use crate::views::GenerationPin;
use time::OffsetDateTime;

/// Whether a different retained commit can name this source occurrence using
/// the supplied hexadecimal prefixes. Reads only metadata in the tenant's
/// view/path; content bytes and unrelated projects are never loaded.
///
/// `hash_prefix` has 1–64 hexadecimal digits and `commit_prefix` 1–12. File
/// validity intervals are half-open generation ranges. Building and failed
/// generations cannot create ambiguity for an otherwise fetchable source.
pub async fn retained_source_commit_collision(
    conn: &mut PgConnection,
    organization: OrganizationId,
    view: ViewId,
    path: &RepoPath,
    hash_prefix: &str,
    commit_prefix: &str,
    expected_commit: &str,
) -> Result<bool, StoreError> {
    validate_commit(expected_commit)?;
    if hash_prefix.is_empty()
        || hash_prefix.len() > 64
        || !hash_prefix.bytes().all(|byte| byte.is_ascii_hexdigit())
        || commit_prefix.is_empty()
        || commit_prefix.len() > 12
        || !commit_prefix.bytes().all(|byte| byte.is_ascii_hexdigit())
    {
        return Err(StoreError::invalid(
            "source prefixes must be bounded hexadecimal strings",
        ));
    }
    Ok(sqlx::query_scalar::<_, bool>(
        "SELECT EXISTS (
           SELECT 1 FROM view_generation g
           JOIN view v ON v.id = g.view_id
           JOIN project p ON p.id = v.project_id AND p.organization_id = $1
           JOIN file_version f ON f.view_id = g.view_id AND f.path = $3
             AND f.valid_from <= g.generation
             AND (f.valid_to IS NULL OR g.generation < f.valid_to)
           WHERE g.view_id = $2 AND g.state IN ('active', 'retired')
             AND left(encode(f.content_hash, 'hex'), length($4::text)) = lower($4)
             AND left(g.resolved_commit, length($5::text)) = lower($5)
             AND g.resolved_commit <> $6
         )",
    )
    .bind(organization)
    .bind(view)
    .bind(path.as_str())
    .bind(hash_prefix)
    .bind(commit_prefix)
    .bind(expected_commit)
    .fetch_one(&mut *conn)
    .await?)
}

/// One file occurrence and its inexpensive content metadata. No redacted
/// text is returned. A legacy file's line count may be unknown until its
/// selected body is loaded; chunk boundaries are not a full-file line count.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileMetadata {
    /// Historical file occurrence visible at the requested generation.
    pub version: FileVersion,
    /// Original content size in bytes, before redaction.
    pub size_bytes: u64,
    /// Redacted source lines (`str::lines` convention), or unknown for legacy
    /// content. An empty stored text has zero lines.
    pub line_count: Option<u32>,
    /// Whether redacted text is stored; does not imply any analysis coverage.
    pub has_text: bool,
}

#[derive(sqlx::FromRow)]
struct MetadataRow {
    path: String,
    valid_from: i64,
    valid_to: Option<i64>,
    content_hash: Vec<u8>,
    language: Option<String>,
    renamed_from: Option<String>,
    size_bytes: Option<i64>,
    redacted_line_count: Option<i64>,
    has_text: bool,
}

impl MetadataRow {
    fn into_metadata(self, pin: GenerationPin) -> Result<FileMetadata, StoreError> {
        let size = self
            .size_bytes
            .ok_or_else(|| StoreError::Corrupt("file metadata refers to missing content".into()))?;
        let line_count = self
            .redacted_line_count
            .map(|value| {
                let value = from_i64(value, "content line count")?;
                u32::try_from(value).map_err(|_| {
                    StoreError::Corrupt(
                        "stored content line count exceeds the supported range".into(),
                    )
                })
            })
            .transpose()?;
        Ok(FileMetadata {
            version: FileVersion {
                view: pin.view,
                path: stored_path(self.path)?,
                valid_from: self.valid_from,
                valid_to: self.valid_to,
                content_hash: hash_from_bytes(&self.content_hash)?,
                language: self.language,
                renamed_from: self.renamed_from.map(stored_path).transpose()?,
            },
            size_bytes: from_i64(size, "content size")?,
            line_count,
            has_text: self.has_text,
        })
    }
}

/// Reads a metadata-only page in bytewise path order at the exact tenant pin.
/// `after` is an exclusive path cursor; `limit` must be 1..=[`BATCH_ROWS`].
/// Foreign pins return no rows. Missing content is corruption, not a zero-byte
/// file. PostgreSQL checks text presence but does not transfer source bodies.
pub async fn files_metadata_at_page(
    conn: &mut PgConnection,
    organization: OrganizationId,
    pin: GenerationPin,
    after: Option<&RepoPath>,
    limit: usize,
) -> Result<Vec<FileMetadata>, StoreError> {
    if !(1..=BATCH_ROWS).contains(&limit) {
        return Err(StoreError::invalid(
            "file metadata page limit must be between 1 and 5000",
        ));
    }
    let rows = sqlx::query_as::<_, MetadataRow>(
        "SELECT f.path, f.valid_from, f.valid_to, f.content_hash, f.language, f.renamed_from,
                c.size_bytes, c.redacted_line_count, (c.redacted_text IS NOT NULL) AS has_text
         FROM file_version f
         JOIN view v ON v.id = f.view_id
         JOIN project p ON p.id = v.project_id AND p.organization_id = $1
         LEFT JOIN content c ON c.organization_id = $1 AND c.hash = f.content_hash
         WHERE f.view_id = $2 AND f.valid_from <= $3
           AND (f.valid_to IS NULL OR f.valid_to > $3)
           AND ($4::text IS NULL OR f.path COLLATE \"C\" > $4 COLLATE \"C\")
         ORDER BY f.path COLLATE \"C\" LIMIT $5",
    )
    .bind(organization)
    .bind(pin.view)
    .bind(pin.generation)
    .bind(after.map(RepoPath::as_str))
    .bind(to_i64(limit as u64, "file metadata page limit")?)
    .fetch_all(conn)
    .await?;
    rows.into_iter().map(|row| row.into_metadata(pin)).collect()
}

/// Reads metadata only for selected paths at a tenant's exact generation pin.
/// Paths are deduplicated and batched; missing paths are absent. A foreign
/// pin returns no rows. Does not load or parse redacted source bodies.
pub async fn files_metadata_in_paths(
    conn: &mut PgConnection,
    organization: OrganizationId,
    pin: GenerationPin,
    paths: &[RepoPath],
) -> Result<Vec<FileMetadata>, StoreError> {
    let unique: Vec<&str> = paths
        .iter()
        .map(RepoPath::as_str)
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect();
    let mut out = Vec::new();
    for batch in unique.chunks(BATCH_ROWS) {
        let rows = sqlx::query_as::<_, MetadataRow>(
            "SELECT f.path, f.valid_from, f.valid_to, f.content_hash, f.language, f.renamed_from,
                    c.size_bytes, c.redacted_line_count, (c.redacted_text IS NOT NULL) AS has_text
             FROM file_version f
             JOIN view v ON v.id = f.view_id
             JOIN project p ON p.id = v.project_id AND p.organization_id = $1
             LEFT JOIN content c ON c.organization_id = $1 AND c.hash = f.content_hash
             WHERE f.view_id = $2 AND f.valid_from <= $3
               AND (f.valid_to IS NULL OR f.valid_to > $3) AND f.path = ANY($4::text[])
             ORDER BY f.path COLLATE \"C\"",
        )
        .bind(organization)
        .bind(pin.view)
        .bind(pin.generation)
        .bind(batch)
        .fetch_all(&mut *conn)
        .await?;
        out.extend(
            rows.into_iter()
                .map(|row| row.into_metadata(pin))
                .collect::<Result<Vec<_>, _>>()?,
        );
    }
    Ok(out)
}

/// Reads one tenant's immutable content without transferring a redacted body
/// larger than `max_redacted_bytes` UTF-8 bytes. Missing content returns `None`;
/// an oversized body is an explicit input error. Non-text content remains a
/// normal `Content` with no body. The limit is enforced inside PostgreSQL.
pub async fn get_content_bounded(
    conn: &mut PgConnection,
    organization: OrganizationId,
    hash: &knowell_core::ContentHash,
    max_redacted_bytes: usize,
) -> Result<Option<Content>, StoreError> {
    #[derive(sqlx::FromRow)]
    struct Row {
        size_bytes: i64,
        language: Option<String>,
        redacted_text: Option<String>,
        created_at: OffsetDateTime,
        body_exceeds_limit: bool,
    }
    let max_bytes = u64::try_from(max_redacted_bytes).map_err(|_| {
        StoreError::invalid("redacted source byte limit exceeds the supported range")
    })?;
    let row = sqlx::query_as::<_, Row>(
        "SELECT size_bytes, language,
                CASE WHEN octet_length(redacted_text) > $3 THEN NULL ELSE redacted_text END AS redacted_text,
                created_at, coalesce(octet_length(redacted_text) > $3, false) AS body_exceeds_limit
         FROM content WHERE organization_id = $1 AND hash = $2",
    )
    .bind(organization)
    .bind(hash_bytes(hash))
    .bind(to_i64(max_bytes, "redacted source byte limit")?)
    .fetch_optional(conn)
    .await?;
    row.map(|row| {
        if row.body_exceeds_limit {
            return Err(StoreError::invalid(
                "stored redacted source exceeds requested byte limit",
            ));
        }
        Ok(Content {
            organization,
            hash: *hash,
            size_bytes: from_i64(row.size_bytes, "content size")?,
            language: row.language,
            redacted_text: row.redacted_text,
            created_at: row.created_at,
        })
    })
    .transpose()
}
