//! Embedding profiles, vectors, nearest-neighbour search and index
//! generations.
//!
//! Vectors of every profile live in one `halfvec` column without a fixed
//! dimension. pgvector's HNSW index needs a fixed dimension (and `halfvec`
//! allows up to 4000, `vector` only 2000, so 3072-dimension profiles need
//! `halfvec`), so each profile gets its own partial expression index
//!
//! ```sql
//! CREATE INDEX embedding_hnsw_<profile> ON embedding
//!   USING hnsw ((embedding::halfvec(<dims>)) halfvec_cosine_ops)
//!   WHERE profile_id = '<profile id>';
//! ```
//!
//! and every query repeats exactly that expression and predicate, with the
//! profile id inlined as a literal so the planner can match the partial
//! index. Vectors of different profiles are never compared: the predicate
//! is part of every similarity query.

use std::collections::BTreeSet;
use std::time::Duration;

use knowell_core::{ContentHash, Name};
use pgvector::Vector;
use sqlx::{AssertSqlSafe, Connection, PgConnection};
use time::OffsetDateTime;

use crate::content::{BATCH_ROWS, split_pins};
use crate::error::{StoreError, Violation, violation};
use crate::hierarchy::stored_name;
use crate::ids::{IndexGenerationId, OrganizationId, ProfileId, ViewId};
use crate::types::{
    GenerationState, from_i32, from_i64, hash_bytes, hash_from_bytes, to_i64, truncate,
};
use crate::views::{GenerationPin, MAX_ERROR_LEN};

/// Largest dimension an HNSW index on `halfvec` supports.
pub const MAX_DIMENSIONS: u32 = 4000;
/// Largest `k` [`nearest`] accepts.
pub const MAX_K: u32 = 1000;
/// pgvector's default `hnsw.ef_search`.
pub const DEFAULT_EF_SEARCH: u32 = 40;
/// Largest finite `halfvec` component magnitude.
const MAX_HALF: f32 = 65_504.0;
/// How long [`register_profile`] waits for another process that is building
/// the same profile's index.
const INDEX_LOCK_WAIT: Duration = Duration::from_secs(600);

/// Settings of a profile to register. Provider, model, dimensions and input
/// format together define it; changing any of them needs a new profile.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewEmbeddingProfile {
    /// Name, unique in the organization.
    pub name: Name,
    /// Provider (`gemini`, `ollama`, `openai-compatible`, ...).
    pub provider: String,
    /// Model id as the provider names it.
    pub model: String,
    /// Vector dimensions, 1..=[`MAX_DIMENSIONS`].
    pub dimensions: u32,
    /// Version of the prepared-input format (prefixes, chunker version).
    pub input_format_version: String,
}

/// A registered, immutable embedding profile.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EmbeddingProfile {
    /// Id.
    pub id: ProfileId,
    /// Tenant.
    pub organization: OrganizationId,
    /// Name.
    pub name: Name,
    /// Provider.
    pub provider: String,
    /// Model.
    pub model: String,
    /// Dimensions.
    pub dimensions: u32,
    /// Prepared-input format version.
    pub input_format_version: String,
    /// Registration time.
    pub created_at: OffsetDateTime,
}

impl EmbeddingProfile {
    /// Name of the profile's partial HNSW index.
    pub fn index_name(&self) -> String {
        format!("embedding_hnsw_{}", self.id.0.simple())
    }

    fn same_settings(&self, spec: &NewEmbeddingProfile) -> bool {
        self.provider == spec.provider
            && self.model == spec.model
            && self.dimensions == spec.dimensions
            && self.input_format_version == spec.input_format_version
    }
}

/// A vector to store for one prepared input.
#[derive(Debug, Clone, PartialEq)]
pub struct NewEmbedding {
    /// Hash of the prepared embedding input.
    pub prepared_input_hash: ContentHash,
    /// The vector, with the profile's dimensions. Stored as `halfvec`
    /// (half precision), so components must be finite and within ±65504.
    pub vector: Vec<f32>,
}

/// Search settings for [`nearest`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NearestOptions {
    /// Number of neighbours, 1..=[`MAX_K`].
    pub k: u32,
    /// HNSW candidate list size (`hnsw.ef_search`); raised to `k` if smaller,
    /// capped at 1000. `None` uses [`DEFAULT_EF_SEARCH`].
    pub ef_search: Option<u32>,
    /// Only return inputs that are chunks of files in these view
    /// generations: per-path inputs recorded with
    /// [`crate::content::replace_chunk_inputs`], or, for file versions
    /// without recorded inputs, the content-level chunk rows. Filtered
    /// searches use pgvector's iterative index scan (`relaxed_order`) so the
    /// filter does not under-fill the result.
    pub scope: Option<Vec<GenerationPin>>,
}

impl NearestOptions {
    /// `k` neighbours, default `ef_search`, no scope.
    pub fn new(k: u32) -> Self {
        Self {
            k,
            ef_search: None,
            scope: None,
        }
    }
}

/// One search hit.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Neighbor {
    /// The prepared input (resolve to file locations with
    /// [`crate::content::locate_chunk_inputs`]).
    pub prepared_input_hash: ContentHash,
    /// Cosine distance (0 = same direction, 2 = opposite).
    pub distance: f64,
}

#[derive(sqlx::FromRow)]
struct ProfileRow {
    id: ProfileId,
    organization_id: OrganizationId,
    name: String,
    provider: String,
    model: String,
    dimensions: i32,
    input_format_version: String,
    created_at_epoch: String,
}

// PostgreSQL timestamps can exceed time's date range. Decode the exact
// numeric epoch instead of letting SQLx construct an unchecked date first.
fn profile_timestamp(epoch: &str) -> Result<OffsetDateTime, StoreError> {
    epoch_nanoseconds(epoch)
        .and_then(|nanos| OffsetDateTime::from_unix_timestamp_nanos(nanos).ok())
        .ok_or_else(|| StoreError::Corrupt("embedding profile registration time is invalid".into()))
}

fn epoch_nanoseconds(epoch: &str) -> Option<i128> {
    if epoch.is_empty() || epoch.len() > 64 {
        return None;
    }
    let negative = epoch.starts_with('-');
    let unsigned = epoch.strip_prefix('-').unwrap_or(epoch);
    let decimal = |digits: &str| {
        if digits.is_empty() {
            return None;
        }
        digits.bytes().try_fold(0_i128, |value, byte| {
            if !byte.is_ascii_digit() {
                return None;
            }
            value.checked_mul(10)?.checked_add(i128::from(byte - b'0'))
        })
    };
    let (whole, fraction) = match unsigned.split_once('.') {
        Some((whole, fraction)) => {
            if fraction.is_empty() || fraction.len() > 9 {
                return None;
            }
            let digits = u32::try_from(fraction.len()).ok()?;
            let scale = 10_i128.checked_pow(9_u32.checked_sub(digits)?)?;
            (whole, decimal(fraction)?.checked_mul(scale)?)
        }
        None => (unsigned, 0),
    };
    let nanos = decimal(whole)?
        .checked_mul(1_000_000_000)?
        .checked_add(fraction)?;
    if negative {
        nanos.checked_neg()
    } else {
        Some(nanos)
    }
}

impl TryFrom<ProfileRow> for EmbeddingProfile {
    type Error = StoreError;

    fn try_from(row: ProfileRow) -> Result<Self, StoreError> {
        Ok(Self {
            id: row.id,
            organization: row.organization_id,
            name: stored_name(row.name)?,
            provider: row.provider,
            model: row.model,
            dimensions: from_i32(row.dimensions, "dimensions")?,
            input_format_version: row.input_format_version,
            created_at: profile_timestamp(&row.created_at_epoch)?,
        })
    }
}

macro_rules! profile_columns {
    () => {
        "id, organization_id, name, provider, model, dimensions, input_format_version,
         EXTRACT(EPOCH FROM created_at)::text AS created_at_epoch"
    };
}

/// Registers a profile and builds its vector index; idempotent.
///
/// Registering the same name with the same settings returns the existing
/// profile (and repairs its index if missing or invalid); the same name with
/// other settings fails with [`StoreError::ProfileConflict`], because
/// profiles are immutable.
///
/// The index is built with `CREATE INDEX CONCURRENTLY`, so `conn` must not be
/// inside a transaction, and concurrent registrations of one profile are
/// serialized with an advisory lock.
pub async fn register_profile(
    conn: &mut PgConnection,
    organization: OrganizationId,
    spec: &NewEmbeddingProfile,
) -> Result<EmbeddingProfile, StoreError> {
    if !(1..=MAX_DIMENSIONS).contains(&spec.dimensions) {
        return Err(StoreError::invalid(format!(
            "dimensions must be 1..={MAX_DIMENSIONS} (the halfvec HNSW limit), got {}",
            spec.dimensions
        )));
    }
    if spec.provider.is_empty() || spec.model.is_empty() || spec.input_format_version.is_empty() {
        return Err(StoreError::invalid(
            "profile provider, model and input format version must not be empty",
        ));
    }
    let dimensions = i32::try_from(spec.dimensions)
        .map_err(|_| StoreError::invalid("dimensions out of range"))?;
    require_available(conn).await?;
    // Concurrent identical inserts can conflict on either unique constraint.
    // Resolve both after the insert, then distinguish name reuse from settings
    // already registered under a different name.
    let inserted = sqlx::query_as::<_, ProfileRow>(concat!(
        "INSERT INTO embedding_profile (organization_id, name, provider, model, dimensions,
                                        input_format_version)
         VALUES ($1, $2, $3, $4, $5, $6)
         ON CONFLICT DO NOTHING
         RETURNING ",
        profile_columns!()
    ))
    .bind(organization)
    .bind(spec.name.as_str())
    .bind(&spec.provider)
    .bind(&spec.model)
    .bind(dimensions)
    .bind(&spec.input_format_version)
    .fetch_optional(&mut *conn)
    .await
    .map_err(|e| match violation(&e) {
        Some(Violation::ForeignKey(_)) => StoreError::not_found("organization", organization),
        _ => StoreError::Database(e),
    })?;
    let profile: EmbeddingProfile = match inserted {
        Some(row) => row.try_into()?,
        None => {
            let existing = find_profile(conn, organization, &spec.name)
                .await?
                .ok_or_else(|| {
                    StoreError::already_exists(
                        "embedding profile with these settings",
                        format!(
                            "{}/{} ({} dims)",
                            spec.provider, spec.model, spec.dimensions
                        ),
                    )
                })?;
            if !existing.same_settings(spec) {
                return Err(StoreError::ProfileConflict {
                    name: spec.name.to_string(),
                });
            }
            existing
        }
    };
    ensure_profile_index(conn, &profile).await?;
    Ok(profile)
}

/// Whether supported vector storage is installed in the current database.
/// This checks live state rather than caching a result across migrations.
pub async fn available(conn: &mut PgConnection) -> Result<bool, StoreError> {
    let version: Option<String> = sqlx::query_scalar(
        "SELECT extversion FROM pg_extension
         WHERE extname = 'vector' AND to_regclass('embedding') IS NOT NULL",
    )
    .fetch_optional(conn)
    .await?;
    Ok(version.as_deref().is_some_and(|text| {
        crate::store::parse_version(text)
            .is_some_and(|v| v >= crate::ServerInfo::MIN_VECTOR_VERSION)
    }))
}

async fn require_available(conn: &mut PgConnection) -> Result<(), StoreError> {
    if available(conn).await? {
        Ok(())
    } else {
        Err(StoreError::SemanticUnavailable)
    }
}

/// Builds (or repairs) the profile's partial HNSW index, serialized per
/// profile with a session advisory lock taken by polling, so a waiting
/// session never holds an open snapshot that `CREATE INDEX CONCURRENTLY`
/// would have to wait for.
async fn ensure_profile_index(
    conn: &mut PgConnection,
    profile: &EmbeddingProfile,
) -> Result<(), StoreError> {
    let lock_key = profile.id.to_string();
    let started = tokio::time::Instant::now();
    loop {
        let locked: bool =
            sqlx::query_scalar("SELECT pg_try_advisory_lock(hashtextextended($1, 7019))")
                .bind(&lock_key)
                .fetch_one(&mut *conn)
                .await?;
        if locked {
            break;
        }
        if started.elapsed() > INDEX_LOCK_WAIT {
            return Err(StoreError::ProfileIndexMissing {
                profile: profile.name.to_string(),
            });
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    let result = build_profile_index(conn, profile).await;
    let unlocked = sqlx::query("SELECT pg_advisory_unlock(hashtextextended($1, 7019))")
        .bind(&lock_key)
        .execute(&mut *conn)
        .await;
    result?;
    unlocked?;
    Ok(())
}

async fn build_profile_index(
    conn: &mut PgConnection,
    profile: &EmbeddingProfile,
) -> Result<(), StoreError> {
    let name = profile.index_name();
    match index_validity(conn, &name).await? {
        Some(true) => return Ok(()),
        Some(false) => {
            // A previous concurrent build was interrupted and left an
            // invalid index behind; it would never be used, so rebuild it.
            tracing::warn!(profile = %profile.name, index = %name, "rebuilding invalid vector index");
            sqlx::query(AssertSqlSafe(format!(
                "DROP INDEX CONCURRENTLY IF EXISTS {name}"
            )))
            .execute(&mut *conn)
            .await?;
        }
        None => {}
    }
    tracing::info!(
        profile = %profile.name,
        dimensions = profile.dimensions,
        index = %name,
        "building vector index"
    );
    // Safe to format: the name and the id are derived from a UUID, the
    // dimension is a validated integer.
    let sql = format!(
        "CREATE INDEX CONCURRENTLY IF NOT EXISTS {name} ON embedding
         USING hnsw ((embedding::halfvec({dims})) halfvec_cosine_ops)
         WHERE profile_id = '{id}'",
        dims = profile.dimensions,
        id = profile.id.0.hyphenated(),
    );
    sqlx::query(AssertSqlSafe(sql)).execute(&mut *conn).await?;
    Ok(())
}

/// `Some(valid)` if the index exists, `None` if it does not.
async fn index_validity(conn: &mut PgConnection, name: &str) -> Result<Option<bool>, StoreError> {
    Ok(sqlx::query_scalar::<_, bool>(
        "SELECT i.indisvalid AND i.indisready FROM pg_index i WHERE i.indexrelid = to_regclass($1)",
    )
    .bind(name)
    .fetch_optional(conn)
    .await?)
}

/// Whether the profile's vector index exists and is valid.
pub async fn profile_index_ready(
    conn: &mut PgConnection,
    profile: &EmbeddingProfile,
) -> Result<bool, StoreError> {
    Ok(index_validity(conn, &profile.index_name()).await? == Some(true))
}

/// Looks a profile up by id.
pub async fn get_profile(
    conn: &mut PgConnection,
    id: ProfileId,
) -> Result<Option<EmbeddingProfile>, StoreError> {
    let row = sqlx::query_as::<_, ProfileRow>(concat!(
        "SELECT ",
        profile_columns!(),
        " FROM embedding_profile WHERE id = $1"
    ))
    .bind(id)
    .fetch_optional(conn)
    .await?;
    row.map(TryInto::try_into).transpose()
}

/// Looks up a profile UUID only within `organization`.
///
/// The tenant predicate is applied before decoding any stored row. Missing
/// UUIDs and UUIDs owned by another organization both return `None`.
pub async fn get_profile_in_organization(
    conn: &mut PgConnection,
    organization: OrganizationId,
    id: ProfileId,
) -> Result<Option<EmbeddingProfile>, StoreError> {
    let row = sqlx::query_as::<_, ProfileRow>(concat!(
        "SELECT ",
        profile_columns!(),
        " FROM embedding_profile WHERE organization_id = $1 AND id = $2"
    ))
    .bind(organization)
    .bind(id)
    .fetch_optional(conn)
    .await?;
    row.map(TryInto::try_into).transpose()
}

/// Looks a profile up by name.
pub async fn find_profile(
    conn: &mut PgConnection,
    organization: OrganizationId,
    name: &Name,
) -> Result<Option<EmbeddingProfile>, StoreError> {
    let row = sqlx::query_as::<_, ProfileRow>(concat!(
        "SELECT ",
        profile_columns!(),
        " FROM embedding_profile WHERE organization_id = $1 AND name = $2"
    ))
    .bind(organization)
    .bind(name.as_str())
    .fetch_optional(conn)
    .await?;
    row.map(TryInto::try_into).transpose()
}

/// Lists an organization's profiles by name.
pub async fn list_profiles(
    conn: &mut PgConnection,
    organization: OrganizationId,
) -> Result<Vec<EmbeddingProfile>, StoreError> {
    sqlx::query_as::<_, ProfileRow>(concat!(
        "SELECT ",
        profile_columns!(),
        " FROM embedding_profile WHERE organization_id = $1 ORDER BY name COLLATE \"C\""
    ))
    .bind(organization)
    .fetch_all(conn)
    .await?
    .into_iter()
    .map(TryInto::try_into)
    .collect()
}

fn check_vector(profile: &EmbeddingProfile, vector: &[f32]) -> Result<(), StoreError> {
    if usize::try_from(profile.dimensions).ok() != Some(vector.len()) {
        return Err(StoreError::DimensionMismatch {
            profile: profile.name.to_string(),
            expected: profile.dimensions,
            actual: vector.len(),
        });
    }
    if vector.iter().any(|v| !v.is_finite() || v.abs() > MAX_HALF) {
        return Err(StoreError::invalid(
            "vector components must be finite and within the half-precision range (±65504)",
        ));
    }
    Ok(())
}

/// Stores vectors for a profile. A vector that already exists for the same
/// prepared input is kept (the cache key is profile x prepared input).
/// Returns how many were new.
pub async fn upsert_embeddings(
    conn: &mut PgConnection,
    profile: &EmbeddingProfile,
    embeddings: &[NewEmbedding],
) -> Result<u64, StoreError> {
    require_available(conn).await?;
    for e in embeddings {
        check_vector(profile, &e.vector)?;
    }
    let mut inserted = 0;
    for batch in embeddings.chunks(BATCH_ROWS) {
        let hashes: Vec<Vec<u8>> = batch
            .iter()
            .map(|e| hash_bytes(&e.prepared_input_hash))
            .collect();
        let vectors: Vec<Vector> = batch
            .iter()
            .map(|e| Vector::from(e.vector.clone()))
            .collect();
        inserted += sqlx::query(
            "INSERT INTO embedding (profile_id, prepared_input_hash, embedding)
             SELECT $1, t.hash, t.v::halfvec FROM unnest($2::bytea[], $3::vector[]) AS t(hash, v)
             ON CONFLICT (profile_id, prepared_input_hash) DO NOTHING",
        )
        .bind(profile.id)
        .bind(&hashes)
        .bind(&vectors)
        .execute(&mut *conn)
        .await?
        .rows_affected();
    }
    Ok(inserted)
}

/// Which of `hashes` have no vector in the profile yet, in input order
/// without duplicates: the inputs that still need an embedding call.
pub async fn missing_embeddings(
    conn: &mut PgConnection,
    profile: ProfileId,
    hashes: &[ContentHash],
) -> Result<Vec<ContentHash>, StoreError> {
    require_available(conn).await?;
    let mut missing = Vec::new();
    for batch in hashes.chunks(BATCH_ROWS) {
        let bytes: Vec<Vec<u8>> = batch.iter().map(hash_bytes).collect();
        let rows: Vec<Vec<u8>> = sqlx::query_scalar(
            "SELECT t.hash FROM unnest($2::bytea[]) WITH ORDINALITY AS t(hash, i)
             WHERE NOT EXISTS (SELECT 1 FROM embedding e
                               WHERE e.profile_id = $1 AND e.prepared_input_hash = t.hash)
             ORDER BY t.i",
        )
        .bind(profile)
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

/// The stored vector (as stored, i.e. rounded to half precision).
pub async fn get_embedding(
    conn: &mut PgConnection,
    profile: ProfileId,
    prepared_input_hash: &ContentHash,
) -> Result<Option<Vec<f32>>, StoreError> {
    require_available(conn).await?;
    let row: Option<Vector> = sqlx::query_scalar(
        "SELECT embedding::vector FROM embedding WHERE profile_id = $1 AND prepared_input_hash = $2",
    )
    .bind(profile)
    .bind(hash_bytes(prepared_input_hash))
    .fetch_optional(conn)
    .await?;
    Ok(row.map(|v| v.to_vec()))
}

/// Builds the search SQL. The profile id and dimension are inlined so the
/// statement matches the profile's partial index expression and predicate.
fn nearest_sql(profile: &EmbeddingProfile, scoped: bool) -> String {
    let dims = profile.dimensions;
    let id = profile.id.0.hyphenated();
    let distance = format!("e.embedding::halfvec({dims}) <=> $1::halfvec({dims})");
    let (pins, filter) = if scoped {
        (
            "pins AS MATERIALIZED (
               SELECT p.view_id, p.generation
               FROM unnest($3::uuid[], $4::bigint[]) AS p(view_id, generation)
               JOIN view v ON v.id = p.view_id
               JOIN project pr ON pr.id = v.project_id AND pr.organization_id = $5
             ),",
            // A hit is in scope when a file of a pinned generation records
            // it as one of its per-path inputs; file versions without any
            // recorded input (indexed before migration 0010) fall back to
            // the content-level chunk rows.
            "AND (EXISTS (
               SELECT 1 FROM chunk_input ci
               JOIN file_version f ON f.view_id = ci.view_id AND f.path = ci.path
                                  AND f.valid_from = ci.file_valid_from
               JOIN pins p ON p.view_id = f.view_id AND f.valid_from <= p.generation
                          AND (f.valid_to IS NULL OR f.valid_to > p.generation)
               WHERE ci.prepared_input_hash = e.prepared_input_hash
             ) OR EXISTS (
               SELECT 1 FROM chunk c
               JOIN file_version f ON f.content_hash = c.content_hash
               JOIN pins p ON p.view_id = f.view_id AND f.valid_from <= p.generation
                          AND (f.valid_to IS NULL OR f.valid_to > p.generation)
               WHERE c.organization_id = $5 AND c.prepared_input_hash = e.prepared_input_hash
                 AND NOT EXISTS (SELECT 1 FROM chunk_input x
                                 WHERE x.view_id = f.view_id AND x.path = f.path
                                   AND x.file_valid_from = f.valid_from)
             ))",
        )
    } else {
        ("", "")
    };
    format!(
        "WITH {pins}
         hits AS MATERIALIZED (
           SELECT e.prepared_input_hash, {distance} AS distance
           FROM embedding e
           WHERE e.profile_id = '{id}' {filter}
           ORDER BY {distance}
           LIMIT $2
         )
         SELECT prepared_input_hash, distance FROM hits
         ORDER BY distance, prepared_input_hash"
    )
}

/// Runs `body` with the search settings applied (`SET LOCAL` in a
/// transaction).
async fn with_search_settings<'c>(
    conn: &'c mut PgConnection,
    profile: &EmbeddingProfile,
    query: &[f32],
    options: &NearestOptions,
) -> Result<(sqlx::Transaction<'c, sqlx::Postgres>, String), StoreError> {
    require_available(conn).await?;
    check_vector(profile, query)?;
    if !(1..=MAX_K).contains(&options.k) {
        return Err(StoreError::invalid(format!(
            "k must be 1..={MAX_K}, got {}",
            options.k
        )));
    }
    if !profile_index_ready(conn, profile).await? {
        return Err(StoreError::ProfileIndexMissing {
            profile: profile.name.to_string(),
        });
    }
    let ef_search = options
        .ef_search
        .unwrap_or(DEFAULT_EF_SEARCH)
        .max(options.k)
        .clamp(1, 1000);
    let iterative = if options.scope.is_some() {
        "relaxed_order"
    } else {
        "off"
    };
    let mut tx = conn.begin().await?;
    sqlx::query(
        "SELECT set_config('hnsw.ef_search', $1, true),
                set_config('hnsw.iterative_scan', $2, true)",
    )
    .bind(ef_search.to_string())
    .bind(iterative)
    .execute(&mut *tx)
    .await?;
    Ok((tx, nearest_sql(profile, options.scope.is_some())))
}

/// The `k` nearest prepared inputs to `query` by cosine distance within one
/// profile, nearest first (ties by hash). Fails with
/// [`StoreError::ProfileIndexMissing`] rather than silently scanning when the
/// profile's index is missing.
pub async fn nearest(
    conn: &mut PgConnection,
    profile: &EmbeddingProfile,
    query: &[f32],
    options: &NearestOptions,
) -> Result<Vec<Neighbor>, StoreError> {
    let (mut tx, sql) = with_search_settings(conn, profile, query, options).await?;
    let (views, generations) = split_pins(options.scope.as_deref().unwrap_or_default());
    let mut q = sqlx::query_as::<_, (Vec<u8>, f64)>(AssertSqlSafe(sql))
        .bind(Vector::from(query.to_vec()))
        .bind(i64::from(options.k));
    if options.scope.is_some() {
        q = q.bind(views).bind(generations).bind(profile.organization);
    }
    let rows = q.fetch_all(&mut *tx).await?;
    tx.commit().await?;
    rows.into_iter()
        .map(|(hash, distance)| {
            Ok(Neighbor {
                prepared_input_hash: hash_from_bytes(&hash)?,
                distance,
            })
        })
        .collect()
}

/// The query plan [`nearest`] would use (`EXPLAIN`), for diagnostics such as
/// checking that the profile's index is used.
pub async fn explain_nearest(
    conn: &mut PgConnection,
    profile: &EmbeddingProfile,
    query: &[f32],
    options: &NearestOptions,
) -> Result<String, StoreError> {
    let (mut tx, sql) = with_search_settings(conn, profile, query, options).await?;
    let (views, generations) = split_pins(options.scope.as_deref().unwrap_or_default());
    let mut q =
        sqlx::query_scalar::<_, String>(AssertSqlSafe(format!("EXPLAIN (COSTS OFF) {sql}")))
            .bind(Vector::from(query.to_vec()))
            .bind(i64::from(options.k));
    if options.scope.is_some() {
        q = q.bind(views).bind(generations).bind(profile.organization);
    }
    let lines = q.fetch_all(&mut *tx).await?;
    tx.commit().await?;
    Ok(lines.join("\n"))
}

/// Which view generation the vectors of a profile cover.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IndexGeneration {
    /// Id.
    pub id: IndexGenerationId,
    /// View.
    pub view: ViewId,
    /// Profile.
    pub profile: ProfileId,
    /// View generation covered.
    pub view_generation: i64,
    /// Building, active, retired or failed.
    pub state: GenerationState,
    /// Chunks the generation has to cover.
    pub chunk_count: u64,
    /// Chunks that have a vector.
    pub embedded_count: u64,
    /// Why it failed.
    pub error: Option<String>,
    /// Creation time.
    pub created_at: OffsetDateTime,
    /// Activation time.
    pub activated_at: Option<OffsetDateTime>,
    /// When it stopped building or was retired.
    pub finished_at: Option<OffsetDateTime>,
}

#[derive(sqlx::FromRow)]
struct IndexGenerationRow {
    id: IndexGenerationId,
    view_id: ViewId,
    profile_id: ProfileId,
    view_generation: i64,
    state: GenerationState,
    chunk_count: i64,
    embedded_count: i64,
    error: Option<String>,
    created_at: OffsetDateTime,
    activated_at: Option<OffsetDateTime>,
    finished_at: Option<OffsetDateTime>,
}

impl TryFrom<IndexGenerationRow> for IndexGeneration {
    type Error = StoreError;

    fn try_from(row: IndexGenerationRow) -> Result<Self, StoreError> {
        Ok(Self {
            id: row.id,
            view: row.view_id,
            profile: row.profile_id,
            view_generation: row.view_generation,
            state: row.state,
            chunk_count: from_i64(row.chunk_count, "chunk count")?,
            embedded_count: from_i64(row.embedded_count, "embedded count")?,
            error: row.error,
            created_at: row.created_at,
            activated_at: row.activated_at,
            finished_at: row.finished_at,
        })
    }
}

macro_rules! index_generation_columns {
    () => {
        "id, view_id, profile_id, view_generation, state, chunk_count, embedded_count, error, created_at, activated_at, finished_at"
    };
}

/// Starts (or returns the existing) index generation for a view generation
/// and a profile.
pub async fn begin_index_generation(
    conn: &mut PgConnection,
    pin: GenerationPin,
    profile: ProfileId,
) -> Result<IndexGeneration, StoreError> {
    let inserted = sqlx::query_as::<_, IndexGenerationRow>(concat!(
        "INSERT INTO index_generation (view_id, profile_id, view_generation) VALUES ($1, $2, $3)
         ON CONFLICT (view_id, profile_id, view_generation) DO NOTHING
         RETURNING ",
        index_generation_columns!()
    ))
    .bind(pin.view)
    .bind(profile)
    .bind(pin.generation)
    .fetch_optional(&mut *conn)
    .await
    .map_err(|e| match violation(&e) {
        Some(Violation::ForeignKey(_)) => StoreError::not_found(
            "view generation or profile",
            format!("{}@{} / {profile}", pin.view, pin.generation),
        ),
        _ => StoreError::Database(e),
    })?;
    if let Some(row) = inserted {
        return row.try_into();
    }
    sqlx::query_as::<_, IndexGenerationRow>(concat!(
        "SELECT ",
        index_generation_columns!(),
        " FROM index_generation WHERE view_id = $1 AND profile_id = $2 AND view_generation = $3"
    ))
    .bind(pin.view)
    .bind(profile)
    .bind(pin.generation)
    .fetch_one(conn)
    .await?
    .try_into()
}

/// Records progress counts of a building index generation.
pub async fn update_index_counts(
    conn: &mut PgConnection,
    id: IndexGenerationId,
    chunk_count: u64,
    embedded_count: u64,
) -> Result<(), StoreError> {
    let done = sqlx::query(
        "UPDATE index_generation SET chunk_count = $2, embedded_count = $3
         WHERE id = $1 AND state = 'building'",
    )
    .bind(id)
    .bind(to_i64(chunk_count, "chunk count")?)
    .bind(to_i64(embedded_count, "embedded count")?)
    .execute(conn)
    .await?;
    if done.rows_affected() == 0 {
        return Err(StoreError::invalid(format!(
            "index generation {id} does not exist or is not building"
        )));
    }
    Ok(())
}

/// Activates a building index generation and retires the previously active
/// one of the same view and profile. Fenced like view generations: it fails
/// with [`StoreError::StaleGeneration`] if the active one covers the same or
/// a newer view generation.
pub async fn activate_index_generation(
    conn: &mut PgConnection,
    id: IndexGenerationId,
) -> Result<IndexGeneration, StoreError> {
    let mut tx = conn.begin().await?;
    let view: ViewId = sqlx::query_scalar("SELECT view_id FROM index_generation WHERE id = $1")
        .bind(id)
        .fetch_optional(&mut *tx)
        .await?
        .ok_or_else(|| StoreError::not_found("index generation", id))?;
    // Serialize activations per view (same lock order as view activation),
    // so the active row read below cannot change before this commits.
    sqlx::query("SELECT 1 FROM view WHERE id = $1 FOR UPDATE")
        .bind(view)
        .execute(&mut *tx)
        .await?;
    let row = sqlx::query_as::<_, IndexGenerationRow>(concat!(
        "SELECT ",
        index_generation_columns!(),
        " FROM index_generation WHERE id = $1 FOR UPDATE"
    ))
    .bind(id)
    .fetch_one(&mut *tx)
    .await?;
    let active: Option<i64> = sqlx::query_scalar(
        "SELECT view_generation FROM index_generation
         WHERE view_id = $1 AND profile_id = $2 AND state = 'active'",
    )
    .bind(row.view_id)
    .bind(row.profile_id)
    .fetch_optional(&mut *tx)
    .await?;
    if let Some(active) = active
        && active >= row.view_generation
    {
        return Err(StoreError::StaleGeneration {
            view: row.view_id,
            generation: row.view_generation,
            active,
        });
    }
    if row.state != GenerationState::Building {
        return Err(StoreError::GenerationNotBuilding {
            view: row.view_id,
            generation: row.view_generation,
            state: row.state,
        });
    }
    sqlx::query(
        "UPDATE index_generation SET state = 'retired', finished_at = now()
         WHERE view_id = $1 AND profile_id = $2 AND state = 'active'",
    )
    .bind(row.view_id)
    .bind(row.profile_id)
    .execute(&mut *tx)
    .await?;
    let activated = sqlx::query_as::<_, IndexGenerationRow>(concat!(
        "UPDATE index_generation SET state = 'active', activated_at = now(), finished_at = now()
         WHERE id = $1 RETURNING ",
        index_generation_columns!()
    ))
    .bind(id)
    .fetch_one(&mut *tx)
    .await?;
    tx.commit().await?;
    activated.try_into()
}

/// Marks a building index generation failed (`error` is truncated to
/// [`MAX_ERROR_LEN`] and must not contain secrets).
pub async fn fail_index_generation(
    conn: &mut PgConnection,
    id: IndexGenerationId,
    error: &str,
) -> Result<(), StoreError> {
    let done = sqlx::query(
        "UPDATE index_generation SET state = 'failed', error = $2, finished_at = now()
         WHERE id = $1 AND state = 'building'",
    )
    .bind(id)
    .bind(truncate(error, MAX_ERROR_LEN))
    .execute(conn)
    .await?;
    if done.rows_affected() == 0 {
        return Err(StoreError::invalid(format!(
            "index generation {id} does not exist or is not building"
        )));
    }
    Ok(())
}

/// Makes a failed index generation building again (its error is cleared),
/// so a later run can complete it; vectors written before stay reused.
/// Returns whether it was failed.
pub async fn restart_index_generation(
    conn: &mut PgConnection,
    id: IndexGenerationId,
) -> Result<bool, StoreError> {
    let done = sqlx::query(
        "UPDATE index_generation SET state = 'building', error = NULL, finished_at = NULL
         WHERE id = $1 AND state = 'failed'",
    )
    .bind(id)
    .execute(conn)
    .await?;
    Ok(done.rows_affected() == 1)
}

/// The index generation of one view generation and profile, in whatever
/// state, if one was started.
pub async fn index_generation_at(
    conn: &mut PgConnection,
    pin: GenerationPin,
    profile: ProfileId,
) -> Result<Option<IndexGeneration>, StoreError> {
    let row = sqlx::query_as::<_, IndexGenerationRow>(concat!(
        "SELECT ",
        index_generation_columns!(),
        " FROM index_generation WHERE view_id = $1 AND profile_id = $2 AND view_generation = $3"
    ))
    .bind(pin.view)
    .bind(profile)
    .bind(pin.generation)
    .fetch_optional(conn)
    .await?;
    row.map(TryInto::try_into).transpose()
}

/// How many of a view generation's chunks have a vector in one profile.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct InputCoverage {
    /// Chunks of the generation's files meant to be embedded (per-path
    /// inputs with `embed` set).
    pub inputs: u64,
    /// Of those, the chunks whose prepared input has a vector in the profile.
    pub embedded: u64,
}

impl InputCoverage {
    /// Whether every input has a vector (also when there are none).
    pub fn is_complete(&self) -> bool {
        self.embedded >= self.inputs
    }
}

/// Embedding coverage of the files visible at `pin`: chunks with a vector in
/// `profile` out of the chunks meant to be embedded, counted over the
/// per-path inputs of `parser_version` (files indexed before per-path inputs
/// existed are not counted). Chunks are counted per path, so identical
/// content at two paths counts twice.
pub async fn input_coverage(
    conn: &mut PgConnection,
    pin: GenerationPin,
    profile: ProfileId,
    parser_version: &str,
) -> Result<InputCoverage, StoreError> {
    require_available(conn).await?;
    let (inputs, embedded): (i64, i64) = sqlx::query_as(
        "SELECT count(*), count(e.prepared_input_hash)
         FROM file_version f
         JOIN chunk_input c ON c.view_id = f.view_id AND c.path = f.path
                           AND c.file_valid_from = f.valid_from
         LEFT JOIN embedding e ON e.profile_id = $3 AND e.prepared_input_hash = c.prepared_input_hash
         WHERE f.view_id = $1 AND f.valid_from <= $2 AND (f.valid_to IS NULL OR f.valid_to > $2)
           AND c.parser_version = $4 AND c.embed",
    )
    .bind(pin.view)
    .bind(pin.generation)
    .bind(profile)
    .bind(parser_version)
    .fetch_one(conn)
    .await?;
    Ok(InputCoverage {
        inputs: from_i64(inputs, "input count")?,
        embedded: from_i64(embedded, "embedded count")?,
    })
}

/// The active index generation of a view and profile, if any.
pub async fn active_index_generation(
    conn: &mut PgConnection,
    view: ViewId,
    profile: ProfileId,
) -> Result<Option<IndexGeneration>, StoreError> {
    let row = sqlx::query_as::<_, IndexGenerationRow>(concat!(
        "SELECT ",
        index_generation_columns!(),
        " FROM index_generation WHERE view_id = $1 AND profile_id = $2 AND state = 'active'"
    ))
    .bind(view)
    .bind(profile)
    .fetch_optional(conn)
    .await?;
    row.map(TryInto::try_into).transpose()
}

#[cfg(test)]
mod tests {
    use super::*;
    use uuid::Uuid;

    fn profile(dims: u32) -> EmbeddingProfile {
        EmbeddingProfile {
            id: ProfileId(Uuid::from_u128(0x0123_4567_89ab_7def_8123_4567_89ab_cdef)),
            organization: OrganizationId(Uuid::nil()),
            name: Name::new("compact").unwrap(),
            provider: "test".into(),
            model: "m".into(),
            dimensions: dims,
            input_format_version: "1".into(),
            created_at: OffsetDateTime::UNIX_EPOCH,
        }
    }

    #[test]
    fn profile_epoch_preserves_exact_signed_fractional_seconds() {
        for (epoch, nanos) in [
            ("0", 0),
            ("-0", 0),
            ("0.000001", 1_000),
            ("-0.000001", -1_000),
            ("1.000001", 1_000_001_000),
            ("-1.000001", -1_000_001_000),
            ("0.123456789", 123_456_789),
            ("-0.123456789", -123_456_789),
            ("1791019845.123456", 1_791_019_845_123_456_000),
            ("-1791019845.123456", -1_791_019_845_123_456_000),
        ] {
            assert_eq!(epoch_nanoseconds(epoch), Some(nanos), "{epoch}");
            assert_eq!(
                profile_timestamp(epoch).unwrap().unix_timestamp_nanos(),
                nanos,
                "{epoch}"
            );
        }
    }

    #[test]
    fn invalid_profile_epochs_fail_without_echoing_stored_input() {
        for epoch in [
            "",
            "-",
            ".",
            "1.",
            ".1",
            "-.1",
            "1.2.3",
            "1.0000000001",
            "+1",
            " 1",
            "1\n",
            "1e6",
            "--1",
            "Infinity",
            "-Infinity",
            "NaN",
            "１２３",
            "KNOWELL_CANARY_FAKE_PROFILE_EPOCH",
            "170141183460469231731687303715884105728",
            "170141183460469231731687303715884105727.999999999",
            "99999999999999999999999999999999999999999999999999999999999999999",
        ] {
            assert_eq!(epoch_nanoseconds(epoch), None);
            let error = profile_timestamp(epoch).unwrap_err();
            assert!(matches!(error, StoreError::Corrupt(_)));
            assert_eq!(
                error.to_string(),
                "stored data is inconsistent: embedding profile registration time is invalid"
            );
        }
    }

    #[test]
    fn profile_epoch_outside_native_timestamp_range_is_rejected() {
        let max = time::Date::MAX.with_time(time::Time::MAX).assume_utc();
        let seconds_beyond_max = max.unix_timestamp() + 1;
        let epoch = seconds_beyond_max.to_string();
        assert!(epoch_nanoseconds(&epoch).is_some());
        assert!(matches!(
            profile_timestamp(&epoch),
            Err(StoreError::Corrupt(_))
        ));
        let min = time::Date::MIN.midnight().assume_utc();
        let seconds_before_min = min.unix_timestamp() - 1;
        let epoch = seconds_before_min.to_string();
        assert!(epoch_nanoseconds(&epoch).is_some());
        assert!(matches!(
            profile_timestamp(&epoch),
            Err(StoreError::Corrupt(_))
        ));
    }

    #[test]
    fn sql_repeats_the_index_expression_and_predicate() {
        let p = profile(768);
        for scoped in [false, true] {
            let sql = nearest_sql(&p, scoped);
            assert!(sql.contains("e.embedding::halfvec(768) <=> $1::halfvec(768)"));
            assert!(sql.contains("e.profile_id = '01234567-89ab-7def-8123-456789abcdef'"));
            assert_eq!(sql.contains("$5"), scoped);
        }
        assert_eq!(
            p.index_name(),
            "embedding_hnsw_0123456789ab7def8123456789abcdef"
        );
    }

    #[test]
    fn vectors_are_checked_against_the_profile() {
        let p = profile(3);
        assert!(check_vector(&p, &[0.1, 0.2, 0.3]).is_ok());
        assert!(matches!(
            check_vector(&p, &[0.1, 0.2]),
            Err(StoreError::DimensionMismatch {
                expected: 3,
                actual: 2,
                ..
            })
        ));
        assert!(check_vector(&p, &[f32::NAN, 0.0, 0.0]).is_err());
        assert!(check_vector(&p, &[f32::INFINITY, 0.0, 0.0]).is_err());
        assert!(check_vector(&p, &[70_000.0, 0.0, 0.0]).is_err());
    }
}
