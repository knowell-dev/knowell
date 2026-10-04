//! Immutable prepared compiler imports and exact-generation analysis coverage.
//!
//! Callers strip source text and secrets before staging compiler data. Coverage
//! is written only while a generation is building and is never inherited by a
//! later generation; failed generations are hidden and pruning deletes their
//! coverage through the generation foreign key.

use std::collections::BTreeSet;
use std::io::{self, Write};

use knowell_core::{ContentHash, RepoPath};
use serde_json::Value;
use sqlx::{Connection, PgConnection};
use uuid::Uuid;

use crate::content::BATCH_ROWS;
use crate::error::StoreError;
use crate::ids::{OrganizationId, ViewId};
use crate::symbols::{NewOccurrence, Scoped, replace_scope};
use crate::types::{hash_bytes, to_i32, validate_commit, validate_generation};
use crate::views::{GenerationPin, lock_building};

/// Largest compact JSON encoding of a prepared import, in bytes (64 MiB).
pub const MAX_SCIP_IMPORT_BYTES: usize = 64 * 1024 * 1024;
/// Largest compact JSON encoding of per-file coverage, in bytes (64 KiB).
pub const MAX_COVERAGE_BYTES: usize = 64 * 1024;
/// Deepest JSON object/array nesting accepted for persisted analysis.
pub const MAX_ANALYSIS_DEPTH: usize = 64;

/// Analysis reported for an exact file and generation, ordered by provider.
#[derive(Debug, Clone, PartialEq)]
pub struct AnalysisCoverage {
    /// `syntax` or `scip`; availability does not establish completeness.
    pub provider: String,
    /// Provider-specific sanitized coverage and provenance, not source text.
    pub details: Value,
}

struct SizeLimit {
    written: usize,
    limit: usize,
}

impl Write for SizeLimit {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        let next = self
            .written
            .checked_add(bytes.len())
            .filter(|next| *next <= self.limit)
            .ok_or_else(|| io::Error::other("analysis json size limit exceeded"))?;
        self.written = next;
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

fn validate_json(value: &Value, limit: usize) -> Result<(), StoreError> {
    if !value.is_object() {
        return Err(StoreError::invalid("analysis data must be a json object"));
    }
    // Bound depth before invoking serde, and reject NULs which PostgreSQL JSONB
    // cannot represent. Errors deliberately never include the supplied data.
    let mut pending = vec![(value, 0usize)];
    while let Some((value, depth)) = pending.pop() {
        if depth > MAX_ANALYSIS_DEPTH {
            return Err(StoreError::invalid("analysis json nesting limit exceeded"));
        }
        match value {
            Value::Object(fields) => {
                if fields.keys().any(|key| key.contains('\0')) {
                    return Err(StoreError::invalid(
                        "analysis json contains a nul character",
                    ));
                }
                pending.extend(fields.values().map(|value| (value, depth + 1)));
            }
            Value::Array(values) => {
                pending.extend(values.iter().map(|value| (value, depth + 1)));
            }
            Value::String(text) if text.contains('\0') => {
                return Err(StoreError::invalid(
                    "analysis json contains a nul character",
                ));
            }
            _ => {}
        }
    }
    serde_json::to_writer(&mut SizeLimit { written: 0, limit }, value)
        .map_err(|_| StoreError::invalid("analysis json size limit exceeded"))
}

fn validate_provider(provider: &str) -> Result<(), StoreError> {
    if matches!(provider, "syntax" | "scip") {
        Ok(())
    } else {
        Err(StoreError::invalid(
            "analysis provider must be syntax or scip",
        ))
    }
}

fn validate_scip_origin(origin: &str) -> Result<(), StoreError> {
    if origin.starts_with("scip:")
        && (6..=1024).contains(&origin.len())
        && !origin.chars().any(char::is_control)
    {
        Ok(())
    } else {
        Err(StoreError::invalid(
            "compiler occurrence origin must start with scip: and contain 6 to 1024 bytes without control characters",
        ))
    }
}

async fn owned_view(
    conn: &mut PgConnection,
    organization: OrganizationId,
    view: ViewId,
) -> Result<(), StoreError> {
    let exists: bool = sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM view v JOIN project p ON p.id = v.project_id
         WHERE v.id = $1 AND p.organization_id = $2)",
    )
    .bind(view)
    .bind(organization)
    .fetch_one(conn)
    .await?;
    if exists {
        Ok(())
    } else {
        Err(StoreError::not_found("view", view))
    }
}

async fn exact_file(
    conn: &mut PgConnection,
    pin: GenerationPin,
    path: &RepoPath,
    hash: &ContentHash,
) -> Result<(), StoreError> {
    let exists: bool = sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM file_version
         WHERE view_id = $1 AND valid_from <= $2 AND (valid_to IS NULL OR valid_to > $2)
           AND path = $3 AND content_hash = $4)",
    )
    .bind(pin.view)
    .bind(pin.generation)
    .bind(path.as_str())
    .bind(hash_bytes(hash))
    .fetch_one(conn)
    .await?;
    if exists {
        Ok(())
    } else {
        Err(StoreError::invalid(
            "analysis file is absent or its content hash does not match the generation",
        ))
    }
}

/// Stages source-free, sanitized compiler analysis for an owned view and exact
/// lowercase Git commit (40 or 64 hexadecimal characters).
///
/// The artifact hash identifies an immutable input. Repeating an identical
/// import returns its id; changing its prepared payload under the same artifact
/// identity fails. Different artifacts remain separately selectable. No later
/// generation automatically adopts an import.
pub async fn stage_scip_import(
    conn: &mut PgConnection,
    organization: OrganizationId,
    view: ViewId,
    source_revision: &str,
    artifact_hash: &ContentHash,
    payload: &Value,
) -> Result<Uuid, StoreError> {
    validate_commit(source_revision)?;
    validate_json(payload, MAX_SCIP_IMPORT_BYTES)?;
    let mut tx = conn.begin().await?;
    owned_view(&mut tx, organization, view).await?;
    sqlx::query(
        "INSERT INTO scip_import (organization_id, view_id, source_revision, artifact_hash, payload)
         VALUES ($1, $2, $3, $4, $5) ON CONFLICT DO NOTHING",
    )
    .bind(organization)
    .bind(view)
    .bind(source_revision)
    .bind(hash_bytes(artifact_hash))
    .bind(payload)
    .execute(&mut *tx)
    .await?;
    let (id, same): (Uuid, bool) = sqlx::query_as(
        "SELECT id, payload = $5 FROM scip_import
         WHERE organization_id = $1 AND view_id = $2 AND source_revision = $3 AND artifact_hash = $4",
    )
    .bind(organization)
    .bind(view)
    .bind(source_revision)
    .bind(hash_bytes(artifact_hash))
    .bind(payload)
    .fetch_one(&mut *tx)
    .await?;
    if !same {
        return Err(StoreError::Conflict {
            entity: "compiler import",
            key: id.to_string(),
            detail: "the artifact has different prepared analysis; use a new artifact identity"
                .into(),
        });
    }
    tx.commit().await?;
    Ok(id)
}

/// Loads an explicitly selected compiler import only for its owning tenant,
/// view and exact expected lowercase Git revision. Missing, foreign and
/// revision-mismatched imports all return `None`.
pub async fn load_scip_import(
    conn: &mut PgConnection,
    organization: OrganizationId,
    view: ViewId,
    id: Uuid,
    expected_revision: &str,
) -> Result<Option<Value>, StoreError> {
    validate_commit(expected_revision)?;
    let payload: Option<Value> = sqlx::query_scalar(
        "SELECT i.payload FROM scip_import i
         JOIN view v ON v.id = i.view_id JOIN project p ON p.id = v.project_id
         WHERE i.id = $1 AND i.organization_id = $2 AND i.view_id = $3
           AND i.source_revision = $4 AND p.organization_id = $2",
    )
    .bind(id)
    .bind(organization)
    .bind(view)
    .bind(expected_revision)
    .fetch_optional(conn)
    .await?;
    if let Some(payload) = &payload {
        validate_json(payload, MAX_SCIP_IMPORT_BYTES).map_err(|_| {
            StoreError::Corrupt("stored compiler import exceeds analysis limits".into())
        })?;
    }
    Ok(payload)
}

/// Writes sanitized coverage for an exact file/hash of an owned, building
/// generation. Retrying replaces only this provider's coverage for the file.
/// Metadata is an object with a compact JSON encoding of at most 64 KiB.
pub async fn upsert_coverage(
    conn: &mut PgConnection,
    organization: OrganizationId,
    pin: GenerationPin,
    path: &RepoPath,
    hash: &ContentHash,
    provider: &str,
    details: &Value,
) -> Result<(), StoreError> {
    validate_provider(provider)?;
    validate_json(details, MAX_COVERAGE_BYTES)?;
    let mut tx = conn.begin().await?;
    owned_view(&mut tx, organization, pin.view).await?;
    lock_building(&mut tx, pin.view, pin.generation).await?;
    exact_file(&mut tx, pin, path, hash).await?;
    sqlx::query(
        "INSERT INTO analysis_coverage
         (organization_id, view_id, generation, path, content_hash, provider, details)
         VALUES ($1, $2, $3, $4, $5, $6, $7)
         ON CONFLICT (organization_id, view_id, generation, path, provider)
         DO UPDATE SET content_hash = EXCLUDED.content_hash, details = EXCLUDED.details",
    )
    .bind(organization)
    .bind(pin.view)
    .bind(pin.generation)
    .bind(path.as_str())
    .bind(hash_bytes(hash))
    .bind(provider)
    .bind(details)
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(())
}

/// Reads coverage for the exact tenant, generation, path and content hash,
/// ordered by provider. Failed generations, foreign/missing sources and stale
/// hashes return no coverage. A matching prior generation is never substituted.
pub async fn coverage_at(
    conn: &mut PgConnection,
    organization: OrganizationId,
    pin: GenerationPin,
    path: &RepoPath,
    hash: &ContentHash,
) -> Result<Vec<AnalysisCoverage>, StoreError> {
    validate_generation(pin.generation)?;
    let rows: Vec<(String, Value)> = sqlx::query_as(
        "SELECT a.provider, a.details FROM analysis_coverage a
         JOIN view v ON v.id = a.view_id JOIN project p ON p.id = v.project_id
         JOIN view_generation g ON g.view_id = a.view_id AND g.generation = a.generation
         JOIN file_version f ON f.view_id = a.view_id AND f.path = a.path
           AND f.content_hash = a.content_hash AND f.valid_from <= a.generation
           AND (f.valid_to IS NULL OR f.valid_to > a.generation)
         WHERE a.organization_id = $1 AND p.organization_id = $1 AND a.view_id = $2
           AND a.generation = $3 AND a.path = $4 AND a.content_hash = $5
           AND g.state <> 'failed' ORDER BY a.provider COLLATE \"C\"",
    )
    .bind(organization)
    .bind(pin.view)
    .bind(pin.generation)
    .bind(path.as_str())
    .bind(hash_bytes(hash))
    .fetch_all(conn)
    .await?;
    rows.into_iter()
        .map(|(provider, details)| {
            validate_provider(&provider)
                .and_then(|()| validate_json(&details, MAX_COVERAGE_BYTES))
                .map_err(|_| {
                    StoreError::Corrupt("stored analysis coverage exceeds analysis limits".into())
                })?;
            Ok(AnalysisCoverage { provider, details })
        })
        .collect()
}

/// Replaces compiler occurrences for one `scip:` origin in an owned, building
/// generation, preserving syntax occurrences and other origins. Each occurrence
/// must refer to an owned project symbol and exact file/hash in this generation.
/// Lines are one-based and inclusive. Returns the number of rows inserted.
pub async fn replace_scip_occurrences(
    conn: &mut PgConnection,
    organization: OrganizationId,
    pin: GenerationPin,
    origin: &str,
    occurrences: &[NewOccurrence],
) -> Result<u64, StoreError> {
    validate_scip_origin(origin)?;
    let mut tx = conn.begin().await?;
    owned_view(&mut tx, organization, pin.view).await?;
    lock_building(&mut tx, pin.view, pin.generation).await?;
    // A file may contain thousands of references to the same symbol. Validate
    // unique source bindings once and symbol ownership in bounded batches.
    let files: BTreeSet<_> = occurrences
        .iter()
        .map(|occurrence| (&occurrence.path, &occurrence.content_hash))
        .collect();
    for (path, hash) in files {
        exact_file(&mut tx, pin, path, hash).await?;
    }
    let symbols: Vec<_> = occurrences
        .iter()
        .map(|occurrence| occurrence.symbol)
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect();
    for batch in symbols.chunks(BATCH_ROWS) {
        let count: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM symbol s JOIN view v ON v.project_id = s.project_id
             WHERE s.id = ANY($1) AND v.id = $2",
        )
        .bind(batch)
        .bind(pin.view)
        .fetch_one(&mut *tx)
        .await?;
        if usize::try_from(count).ok() != Some(batch.len()) {
            return Err(StoreError::invalid(
                "compiler occurrence symbol does not belong to the view's project",
            ));
        }
    }
    replace_scope(
        &mut tx,
        Scoped::ScipOccurrence,
        pin.view,
        pin.generation,
        &[origin],
    )
    .await?;
    let mut written = 0;
    for batch in occurrences.chunks(BATCH_ROWS) {
        let symbols: Vec<_> = batch.iter().map(|occurrence| occurrence.symbol).collect();
        let paths: Vec<_> = batch
            .iter()
            .map(|occurrence| occurrence.path.as_str())
            .collect();
        let hashes: Vec<_> = batch
            .iter()
            .map(|occurrence| hash_bytes(&occurrence.content_hash))
            .collect();
        let starts: Vec<i32> = batch
            .iter()
            .map(|occurrence| to_i32(occurrence.lines.start(), "line"))
            .collect::<Result<_, _>>()?;
        let ends: Vec<i32> = batch
            .iter()
            .map(|occurrence| to_i32(occurrence.lines.end(), "line"))
            .collect::<Result<_, _>>()?;
        let roles: Vec<_> = batch.iter().map(|occurrence| occurrence.role).collect();
        written += sqlx::query(
            "INSERT INTO occurrence (view_id, valid_from, origin, symbol_id, path, content_hash,
                                     start_line, end_line, role)
             SELECT $1, $2, $3, u.* FROM unnest($4::uuid[], $5::text[], $6::bytea[],
                                             $7::integer[], $8::integer[], $9::occurrence_role[]) AS u",
        )
        .bind(pin.view).bind(pin.generation).bind(origin).bind(&symbols).bind(&paths)
        .bind(&hashes).bind(&starts).bind(&ends).bind(&roles)
        .execute(&mut *tx).await?.rows_affected();
    }
    tx.commit().await?;
    Ok(written)
}

/// Removes compiler edges and occurrences from an owned building generation:
/// rows born here are deleted, older open rows are closed here. Syntax products
/// and retained historical generations are preserved. Compiler coverage for
/// this exact generation is deleted in the same transaction, so retries never
/// retain claims about removed products. Returns affected edges.
/// Call before applying an explicitly selected import for each new generation.
pub async fn clear_scip_edges(
    conn: &mut PgConnection,
    organization: OrganizationId,
    pin: GenerationPin,
) -> Result<u64, StoreError> {
    let mut tx = conn.begin().await?;
    owned_view(&mut tx, organization, pin.view).await?;
    lock_building(&mut tx, pin.view, pin.generation).await?;
    let mut edges = 0;
    for (is_edge, query) in [
        (
            true,
            "DELETE FROM edge WHERE view_id = $1 AND valid_from = $2 AND left(origin, 5) = 'scip:'",
        ),
        (
            true,
            "UPDATE edge SET valid_to = $2 WHERE view_id = $1 AND valid_from < $2 AND valid_to IS NULL AND left(origin, 5) = 'scip:'",
        ),
        (
            false,
            "DELETE FROM occurrence WHERE view_id = $1 AND valid_from = $2 AND left(origin, 5) = 'scip:'",
        ),
        (
            false,
            "UPDATE occurrence SET valid_to = $2 WHERE view_id = $1 AND valid_from < $2 AND valid_to IS NULL AND left(origin, 5) = 'scip:'",
        ),
    ] {
        let affected = sqlx::query(query)
            .bind(pin.view)
            .bind(pin.generation)
            .execute(&mut *tx)
            .await?
            .rows_affected();
        if is_edge {
            edges += affected;
        }
    }
    sqlx::query(
        "DELETE FROM analysis_coverage WHERE organization_id = $1 AND view_id = $2
         AND generation = $3 AND provider = 'scip'",
    )
    .bind(organization)
    .bind(pin.view)
    .bind(pin.generation)
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(edges)
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn analysis_json_rejects_nonobjects_nuls_depth_and_size_without_echoing_input() {
        for value in [Value::Null, json!([]), json!("KNOWELL_CANARY_FAKE")] {
            assert!(validate_json(&value, 100).is_err());
        }
        for value in [
            json!({"secret": "KNOWELL_CANARY_FAKE\u{0}"}),
            json!({"\u{0}": 1}),
        ] {
            let error = validate_json(&value, 100).unwrap_err().to_string();
            assert!(!error.contains("KNOWELL_CANARY_FAKE"));
        }
        let value = json!({"x": "abc"});
        let size = serde_json::to_vec(&value).unwrap().len();
        assert!(validate_json(&value, size).is_ok());
        assert!(validate_json(&value, size - 1).is_err());
        let mut deep = json!({});
        for _ in 0..=MAX_ANALYSIS_DEPTH {
            deep = json!({"nested": deep});
        }
        assert!(validate_json(&deep, MAX_COVERAGE_BYTES).is_err());
    }

    #[test]
    fn providers_and_compiler_origins_are_bounded_and_do_not_echo_rejected_values() {
        for provider in ["syntax", "scip"] {
            assert!(validate_provider(provider).is_ok());
        }
        assert!(validate_provider("KNOWELL_CANARY_FAKE").is_err());
        assert!(validate_scip_origin("scip:src/lib.rs").is_ok());
        for origin in ["syntax", "scip:", "scip:x\n", "scip:x\0"] {
            assert!(validate_scip_origin(origin).is_err());
        }
        assert!(validate_scip_origin(&format!("scip:{}", "x".repeat(1020))).is_err());
    }
}
