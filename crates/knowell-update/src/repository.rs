//! Bounded TUF verification with a persistent, generation-based trust datastore.
//!
//! The caller must place the datastore inside its validated, private installation root.
//! Downloads are written only to a caller-owned staging file and must never be executed
//! until this module returns success after consuming the verified stream to EOF.

use std::collections::BTreeSet;
use std::fs::{File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use async_trait::async_trait;
use bytes::Bytes;
use futures::{StreamExt, stream};
use serde::de::{DeserializeSeed, MapAccess, SeqAccess, Visitor};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tough::{
    ExpirationEnforcement, RepositoryLoader, Transport, TransportError, TransportErrorKind,
    TransportStream,
};
use url::Url;

use crate::manifest::{
    Artifact, Channel, MAX_TARGET_BYTES, ReleaseMetadata, ReleaseTarget, Selection, select_release,
};
use crate::{Error, Result};

const METADATA_BYTES: u64 = 8 * 1024 * 1024;
const MAX_REQUESTS: u64 = 256;
const REQUEST_SECONDS: u64 = 120;
const MAX_DATASTORE_FILES: usize = 128;
// Tough's root-update loop adds its configured limit to the trusted version.
// Reserve ample arithmetic headroom before any untrusted root reaches that loop.
const MAX_ROOT_VERSION: u64 = u64::MAX - 1024;

/// Repository configuration. The root bytes are a public out-of-band trust anchor.
#[derive(Clone)]
pub struct RepositoryConfig {
    /// Embedded or explicitly operator-provided signed root; empty is never a trust root.
    pub trusted_root: Option<Vec<u8>>,
    /// Credential-free HTTPS metadata base, or a local directory URL in offline mode.
    pub metadata_url: Url,
    /// Credential-free HTTPS raw-target base, or a local directory URL in offline mode.
    pub targets_url: Url,
    /// Private persistent metadata directory, scoped to this configured repository.
    pub datastore: PathBuf,
    /// Explicit offline mode; both bases must be local `file:` URLs.
    pub offline: bool,
    /// Explicit recovery of an interrupted, structurally valid metadata generation.
    pub recover_pending: bool,
    /// Maximum raw executable size, in bytes; must be at most 1 GiB.
    pub max_target_bytes: u64,
}

impl std::fmt::Debug for RepositoryConfig {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("RepositoryConfig")
            .field(
                "trust_configured",
                &self
                    .trusted_root
                    .as_ref()
                    .is_some_and(|root| !root.is_empty()),
            )
            .field("offline", &self.offline)
            .field("max_target_bytes", &self.max_target_bytes)
            .finish_non_exhaustive()
    }
}

/// Verified metadata and its admission lock, retained through target downloads.
pub struct TrustedRepository {
    releases: Vec<ReleaseTarget>,
    config: RepositoryConfig,
    operation: tokio::sync::Mutex<()>,
    _metadata_lock: File,
}

impl std::fmt::Debug for TrustedRepository {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("TrustedRepository")
            .field("release_count", &self.releases.len())
            .field("max_target_bytes", &self.config.max_target_bytes)
            .finish_non_exhaustive()
    }
}

impl TrustedRepository {
    /// Loads and verifies TUF metadata with expiry and rollback checks enabled.
    ///
    /// Failed later-role verification still preserves earlier accepted root/timestamp
    /// advances. Corrupt or interrupted trust state is never automatically discarded.
    pub async fn load(config: RepositoryConfig) -> Result<Self> {
        Self::load_policy(config, false).await
    }

    /// Explicitly relocates repository endpoints while preserving all accepted trust state.
    ///
    /// The original bootstrap bytes must remain exactly identical. Endpoint identity is
    /// changed only after successful verification; failure still persists newly accepted
    /// root, metadata high-water, and clock advances without resetting any generation.
    pub async fn configure(config: RepositoryConfig) -> Result<Self> {
        Self::load_policy(config, true).await
    }

    async fn load_policy(config: RepositoryConfig, relocate: bool) -> Result<Self> {
        let bootstrap = config
            .trusted_root
            .as_ref()
            .filter(|root| !root.is_empty())
            .ok_or(Error::TrustUnconfigured)?;
        if u64::try_from(bootstrap.len()).map_err(|_| Error::Limit)? > METADATA_BYTES {
            return Err(Error::Limit);
        }
        validate_root(bootstrap)?;
        validate_base(&config.metadata_url, config.offline)?;
        validate_base(&config.targets_url, config.offline)?;
        if config.max_target_bytes == 0 || config.max_target_bytes > MAX_TARGET_BYTES {
            return Err(Error::Limit);
        }
        create_private_directory(&config.datastore)?;
        let metadata_lock = lock_metadata(&config.datastore)?;
        let relocated = pin_source(&config, bootstrap, relocate)?;
        let (repository, generation, sequence) = stage_metadata(&config).await?;
        let releases = parse_releases(&repository, config.max_target_bytes);
        commit_generation(&config.datastore, &generation, sequence)?;
        let releases = releases?;
        if let Some(pin) = relocated {
            replace_source_pin(&config.datastore, &pin)?
        }
        Ok(Self {
            releases,
            config,
            operation: tokio::sync::Mutex::new(()),
            _metadata_lock: metadata_lock,
        })
    }

    /// Returns the signed engine descriptors, sorted by version, platform, and name.
    pub fn releases(&self) -> &[ReleaseTarget] {
        &self.releases
    }

    /// Selects exactly the requested platform, channel, and version policy.
    pub fn select(
        &self,
        current: &semver::Version,
        target: &str,
        channel: Channel,
        selection: &Selection,
        allow_downgrade: bool,
    ) -> Result<ReleaseTarget> {
        select_release(
            &self.releases,
            current,
            target,
            channel,
            selection,
            allow_downgrade,
        )
    }

    /// Streams a signed raw artifact into an empty caller-owned staging file.
    ///
    /// Success means exact length and SHA-256 verification completed at EOF and the
    /// staging file was synced. On error, the caller must discard that staging file.
    pub async fn download(&self, artifact: &Artifact, staging: &mut tokio::fs::File) -> Result<()> {
        if artifact.size == 0 || artifact.size > self.config.max_target_bytes {
            return Err(Error::Limit);
        }
        let planned = self
            .releases
            .iter()
            .find(|release| release.artifact == *artifact || release.metadata.launcher == *artifact)
            .ok_or(Error::Integrity)?;
        if staging.metadata().await?.len() != 0 {
            return Err(Error::State("target staging file must be empty"));
        }
        let _operation = self.operation.lock().await;
        // A fresh generation keeps Tough's clock writes away from committed state.
        // Reverification also detects a release revoked since the initial plan.
        let (repository, generation, sequence) = stage_metadata(&self.config).await?;
        let result = async {
            let releases = parse_releases(&repository, self.config.max_target_bytes)?;
            let fresh = releases
                .iter()
                .find(|release| release.artifact.name == planned.artifact.name)
                .ok_or(Error::Revoked)?;
            if fresh.metadata.revoked {
                return Err(Error::Revoked);
            }
            // A plan binds the whole signed contract, including compatibility and
            // paired launcher identity, even when the engine bytes remain identical.
            if fresh != planned {
                return Err(Error::Integrity);
            }
            tokio::time::timeout(
                Duration::from_secs(REQUEST_SECONDS),
                self.stream_target(&repository, artifact, staging),
            )
            .await
            .map_err(|_| Error::Limit)?
        }
        .await;
        // Even a failed target read may have accepted newer metadata or sampled time.
        commit_generation(&self.config.datastore, &generation, sequence)?;
        result
    }

    async fn stream_target(
        &self,
        repository: &tough::Repository,
        artifact: &Artifact,
        staging: &mut tokio::fs::File,
    ) -> Result<()> {
        let name = tough::TargetName::new(&artifact.name).map_err(|_| Error::Manifest)?;
        let target_stream = repository
            .read_target(&name)
            .await
            .map_err(|_| Error::Metadata)?
            .ok_or(Error::ReleaseUnavailable)?;
        futures::pin_mut!(target_stream);
        let mut size = 0_u64;
        let mut digest = Sha256::new();
        while let Some(chunk) = target_stream.next().await {
            let chunk = chunk.map_err(|_| Error::Integrity)?;
            size = size
                .checked_add(u64::try_from(chunk.len()).map_err(|_| Error::Limit)?)
                .ok_or(Error::Limit)?;
            if size > artifact.size || size > self.config.max_target_bytes {
                return Err(Error::Limit);
            }
            digest.update(&chunk);
            staging.write_all(&chunk).await?;
        }
        if size != artifact.size || hex_digest(digest.finalize().as_slice()) != artifact.sha256 {
            return Err(Error::Integrity);
        }
        staging.sync_all().await?;
        Ok(())
    }
}

async fn stage_metadata(config: &RepositoryConfig) -> Result<(tough::Repository, PathBuf, u64)> {
    let bootstrap = config
        .trusted_root
        .as_ref()
        .ok_or(Error::TrustUnconfigured)?;
    let (generation, sequence) = if config.recover_pending {
        resume_generation(&config.datastore)?
    } else {
        begin_generation(&config.datastore)?
    };
    let root_path = generation.join("root.json");
    let root = if root_path.exists() {
        read_small(&root_path)?
    } else {
        bootstrap.clone()
    };
    validate_root(&root)?;
    let loader = RepositoryLoader::new(
        &root,
        config.metadata_url.clone(),
        config.targets_url.clone(),
    )
    .transport(BoundedTransport::new(config)?)
    .datastore(&generation)
    .expiration_enforcement(ExpirationEnforcement::Safe)
    .limits(tough::Limits {
        max_root_size: METADATA_BYTES,
        max_targets_size: METADATA_BYTES,
        max_timestamp_size: METADATA_BYTES,
        max_snapshot_size: METADATA_BYTES,
        max_root_updates: 64,
    })
    .load();
    let loaded = tokio::time::timeout(Duration::from_secs(REQUEST_SECONDS), loader).await;
    match loaded {
        Ok(Ok(repository)) => Ok((repository, generation, sequence)),
        error => {
            // Preserve each role Tough accepted before a later role failed.
            commit_generation(&config.datastore, &generation, sequence)?;
            Err(if error.is_err() {
                Error::Limit
            } else {
                Error::Metadata
            })
        }
    }
}

#[derive(Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
struct SourcePin {
    format_version: u32,
    bootstrap_sha256: String,
    metadata_url_sha256: String,
    targets_url_sha256: String,
    offline: bool,
}

fn pin_source(
    config: &RepositoryConfig,
    bootstrap: &[u8],
    relocate: bool,
) -> Result<Option<SourcePin>> {
    let pin = SourcePin {
        format_version: 1,
        bootstrap_sha256: hex_digest(&Sha256::digest(bootstrap)),
        metadata_url_sha256: hex_digest(&Sha256::digest(config.metadata_url.as_str().as_bytes())),
        targets_url_sha256: hex_digest(&Sha256::digest(config.targets_url.as_str().as_bytes())),
        offline: config.offline,
    };
    let path = config.datastore.join("source.json");
    if path.exists() {
        let bytes = read_small(&path)?;
        strict_json(&bytes).map_err(|_| Error::State("corrupt update source identity"))?;
        let existing: SourcePin = serde_json::from_slice(&bytes)
            .map_err(|_| Error::State("corrupt update source identity"))?;
        if existing.format_version != 1
            || [
                &existing.bootstrap_sha256,
                &existing.metadata_url_sha256,
                &existing.targets_url_sha256,
            ]
            .iter()
            .any(|digest| !crate::manifest::valid_digest(digest))
        {
            return Err(Error::State("corrupt update source identity"));
        }
        if existing != pin {
            if relocate && existing.bootstrap_sha256 == pin.bootstrap_sha256 {
                return Ok(Some(pin));
            }
            return Err(Error::State(
                "repository identity changed; select a separate trusted datastore",
            ));
        }
    } else {
        for entry in std::fs::read_dir(&config.datastore)? {
            if entry?.file_name().to_string_lossy().starts_with("commit-") {
                return Err(Error::State("repository source identity is missing"));
            }
        }
        publish_small(
            &path,
            &serde_json::to_vec(&pin)
                .map_err(|_| Error::State("cannot encode update source identity"))?,
        )?;
    }
    Ok(None)
}

fn replace_source_pin(datastore: &Path, pin: &SourcePin) -> Result<()> {
    let destination = datastore.join("source.json");
    ensure_regular_path(&destination)?;
    let bytes = serde_json::to_vec(pin)
        .map_err(|_| Error::State("cannot encode update source identity"))?;
    let temporary = datastore.join(format!(".source-{}", uuid::Uuid::now_v7()));
    let mut file = OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(&temporary)?;
    file.write_all(&bytes)?;
    file.sync_all()?;
    drop(file);
    std::fs::rename(temporary, destination)?;
    sync_directory(datastore)
}

fn parse_releases(repository: &tough::Repository, maximum: u64) -> Result<Vec<ReleaseTarget>> {
    let mut releases = Vec::new();
    // Delegations are intentionally outside this first manifest format's trust policy.
    if repository.targets().signed.delegations.is_some() {
        return Err(Error::Manifest);
    }
    for (name, target) in repository.all_targets() {
        let Some(value) = target.custom.get("knowell") else {
            continue;
        };
        if target.custom.len() != 1 || name.raw() != name.resolved() {
            return Err(Error::Manifest);
        }
        let metadata: ReleaseMetadata =
            serde_json::from_value(value.clone()).map_err(|_| Error::Manifest)?;
        metadata.validate(name.raw())?;
        let artifact = Artifact {
            name: name.raw().to_owned(),
            size: target.length,
            sha256: hex_digest(target.hashes.sha256.as_ref()),
        };
        artifact.validate(
            &metadata.version,
            &metadata.target,
            crate::manifest::Component::Engine,
        )?;
        if artifact.size > maximum || metadata.launcher.size > maximum {
            return Err(Error::Limit);
        }
        let launcher_name =
            tough::TargetName::new(&metadata.launcher.name).map_err(|_| Error::Manifest)?;
        let launcher = repository
            .targets()
            .signed
            .targets
            .get(&launcher_name)
            .ok_or(Error::Manifest)?;
        if launcher.length != metadata.launcher.size
            || hex_digest(launcher.hashes.sha256.as_ref()) != metadata.launcher.sha256
        {
            return Err(Error::Integrity);
        }
        releases.push(ReleaseTarget { artifact, metadata });
    }
    releases.sort_by(|left, right| {
        left.metadata
            .version
            .cmp(&right.metadata.version)
            .then_with(|| left.metadata.target.cmp(&right.metadata.target))
            .then_with(|| left.artifact.name.cmp(&right.artifact.name))
    });
    Ok(releases)
}

#[derive(Clone)]
struct BoundedTransport {
    client: reqwest::Client,
    bases: [Url; 2],
    offline: bool,
    maximum: u64,
    requests: Arc<AtomicU64>,
}

impl std::fmt::Debug for BoundedTransport {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("BoundedTransport")
            .field("offline", &self.offline)
            .finish_non_exhaustive()
    }
}

impl BoundedTransport {
    fn new(config: &RepositoryConfig) -> Result<Self> {
        let client = reqwest::Client::builder()
            .https_only(true)
            .connect_timeout(Duration::from_secs(15))
            .timeout(Duration::from_secs(REQUEST_SECONDS))
            .redirect(reqwest::redirect::Policy::custom(|attempt| {
                if attempt.previous().len() >= 5
                    || attempt.url().scheme() != "https"
                    || !attempt.url().username().is_empty()
                    || attempt.url().password().is_some()
                {
                    attempt.stop()
                } else {
                    attempt.follow()
                }
            }))
            .build()
            .map_err(|_| Error::Source)?;
        Ok(Self {
            client,
            bases: [config.metadata_url.clone(), config.targets_url.clone()],
            offline: config.offline,
            maximum: config.max_target_bytes,
            requests: Arc::new(AtomicU64::new(0)),
        })
    }

    fn allowed(&self, url: &Url) -> bool {
        self.bases.iter().any(|base| {
            url.scheme() == base.scheme()
                && url.host_str() == base.host_str()
                && url.port_or_known_default() == base.port_or_known_default()
                && url.path().starts_with(base.path())
                && url.username().is_empty()
                && url.password().is_none()
                && url.query().is_none()
                && url.fragment().is_none()
        })
    }

    async fn fetch_inner(&self, url: Url) -> std::result::Result<TransportStream, TransportError> {
        if !self.allowed(&url) || self.requests.fetch_add(1, Ordering::Relaxed) >= MAX_REQUESTS {
            return Err(transport_error(TransportErrorKind::Other));
        }
        let metadata = url.path().ends_with(".json");
        let root_metadata = url.path().ends_with(".root.json");
        let maximum = if metadata {
            METADATA_BYTES
        } else {
            self.maximum
        };
        let url = if metadata {
            url
        } else {
            rewrite_consistent_target(&url, &self.bases[1])
                .map_err(|_| transport_error(TransportErrorKind::Other))?
        };
        let raw: TransportStream = if self.offline {
            let path = url
                .to_file_path()
                .map_err(|_| transport_error(TransportErrorKind::Other))?;
            if !path.exists() {
                return Err(transport_error(TransportErrorKind::FileNotFound));
            }
            ensure_regular_path(&path).map_err(|_| transport_error(TransportErrorKind::Other))?;
            let file = tokio::fs::File::open(path).await.map_err(|error| {
                transport_error(if error.kind() == std::io::ErrorKind::NotFound {
                    TransportErrorKind::FileNotFound
                } else {
                    TransportErrorKind::Other
                })
            })?;
            if file
                .metadata()
                .await
                .map_err(|_| transport_error(TransportErrorKind::Other))?
                .len()
                > maximum
            {
                return Err(transport_error(TransportErrorKind::Other));
            }
            Box::pin(stream::try_unfold(
                (file, 0_u64),
                move |(mut file, total)| async move {
                    let mut buffer = vec![0_u8; 64 * 1024];
                    let count = file
                        .read(&mut buffer)
                        .await
                        .map_err(|_| transport_error(TransportErrorKind::Other))?;
                    if count == 0 {
                        return Ok(None);
                    }
                    let total = total
                        .checked_add(
                            u64::try_from(count)
                                .map_err(|_| transport_error(TransportErrorKind::Other))?,
                        )
                        .filter(|total| *total <= maximum)
                        .ok_or_else(|| transport_error(TransportErrorKind::Other))?;
                    buffer.truncate(count);
                    Ok(Some((Bytes::from(buffer), (file, total))))
                },
            ))
        } else {
            let response = self
                .client
                .get(url)
                .send()
                .await
                .map_err(|_| transport_error(TransportErrorKind::Other))?;
            if response.status() == reqwest::StatusCode::NOT_FOUND {
                return Err(transport_error(TransportErrorKind::FileNotFound));
            }
            if !response.status().is_success()
                || response
                    .content_length()
                    .is_some_and(|length| length > maximum)
            {
                return Err(transport_error(TransportErrorKind::Other));
            }
            let body = response.bytes_stream();
            Box::pin(stream::try_unfold(
                (Box::pin(body), 0_u64),
                move |(mut body, total)| async move {
                    let Some(chunk) = body.next().await else {
                        return Ok(None);
                    };
                    let chunk = chunk.map_err(|_| transport_error(TransportErrorKind::Other))?;
                    let total = total
                        .checked_add(
                            u64::try_from(chunk.len())
                                .map_err(|_| transport_error(TransportErrorKind::Other))?,
                        )
                        .filter(|total| *total <= maximum)
                        .ok_or_else(|| transport_error(TransportErrorKind::Other))?;
                    Ok(Some((chunk, (body, total))))
                },
            ))
        };
        if metadata {
            let mut raw = raw;
            let mut bytes = Vec::new();
            while let Some(chunk) = raw.next().await {
                bytes.extend_from_slice(&chunk?);
            }
            strict_json(&bytes).map_err(|_| transport_error(TransportErrorKind::Other))?;
            if root_metadata {
                validate_root(&bytes).map_err(|_| transport_error(TransportErrorKind::Other))?;
            }
            Ok(Box::pin(stream::once(
                async move { Ok(Bytes::from(bytes)) },
            )))
        } else {
            Ok(raw)
        }
    }
}

#[async_trait]
impl Transport for BoundedTransport {
    async fn fetch(&self, url: Url) -> std::result::Result<TransportStream, TransportError> {
        match self.fetch_inner(url).await {
            // Tough treats an error returned before the root stream exists as a missing
            // next root. Surface every non-404 failure *inside* the stream instead.
            Err(error) if error.kind() != TransportErrorKind::FileNotFound => {
                Ok(Box::pin(stream::once(async move { Err(error) })))
            }
            result => result,
        }
    }
}

fn transport_error(kind: TransportErrorKind) -> TransportError {
    TransportError::new(kind, "redacted update source")
}

fn rewrite_consistent_target(url: &Url, base: &Url) -> Result<Url> {
    let relative = url.path().strip_prefix(base.path()).ok_or(Error::Source)?;
    let Some((first, basename)) = relative.split_once('/') else {
        return Err(Error::Manifest);
    };
    if first.starts_with('v') {
        return Ok(url.clone());
    }
    let Some((digest, version)) = first.split_once('.') else {
        return Ok(url.clone());
    };
    if !crate::manifest::valid_digest(digest)
        || !version.starts_with('v')
        || basename.contains('/')
        || basename.contains('%')
        || basename.is_empty()
    {
        return Err(Error::Manifest);
    }
    base.join(&format!("{version}/{digest}.{basename}"))
        .map_err(|_| Error::Source)
}

fn validate_base(url: &Url, offline: bool) -> Result<()> {
    let scheme = if offline { "file" } else { "https" };
    if url.scheme() != scheme
        || !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
        || !url.path().ends_with('/')
        || (offline && url.host_str().is_some())
        || (!offline && url.host_str().is_none())
    {
        return Err(Error::Source);
    }
    if offline {
        ensure_directory(&url.to_file_path().map_err(|_| Error::Source)?)?;
    }
    Ok(())
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Commit {
    format_version: u32,
    generation: u64,
}

fn lock_metadata(datastore: &Path) -> Result<File> {
    let path = datastore.join("metadata.lock");
    if path.exists() {
        ensure_regular_path(&path)?
    }
    let file = OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(path)?;
    file.try_lock().map_err(|error| match error {
        std::fs::TryLockError::WouldBlock => Error::Busy,
        std::fs::TryLockError::Error(error) => Error::Io(error),
    })?;
    Ok(file)
}

fn begin_generation(datastore: &Path) -> Result<(PathBuf, u64)> {
    if datastore.join("pending.json").exists() {
        return Err(Error::RecoveryRequired);
    }
    let mut commits = Vec::new();
    for entry in std::fs::read_dir(datastore)? {
        let entry = entry?;
        let name = entry
            .file_name()
            .into_string()
            .map_err(|_| Error::State("unrecognized metadata state file"))?;
        if let Some(sequence) = name
            .strip_prefix("commit-")
            .and_then(|name| name.strip_suffix(".json"))
        {
            let sequence = sequence
                .parse::<u64>()
                .map_err(|_| Error::State("invalid metadata generation"))?;
            if name != format!("commit-{sequence:020}.json") || sequence == 0 {
                return Err(Error::State("invalid metadata generation"));
            }
            commits.push((sequence, entry.path()));
        }
    }
    commits.sort_by_key(|(sequence, _)| *sequence);
    if commits.len() > 64 {
        return Err(Error::Limit);
    }
    let active = if let Some((sequence, path)) = commits.last() {
        let commit: Commit = serde_json::from_slice(&read_small(path)?)
            .map_err(|_| Error::State("corrupt committed metadata pointer"))?;
        if commit.format_version != 1 || commit.generation != *sequence {
            return Err(Error::State("corrupt committed metadata pointer"));
        }
        let active = datastore.join(format!("generation-{sequence:020}"));
        validate_generation(&active)?;
        Some((*sequence, active))
    } else {
        None
    };
    let next = active.as_ref().map_or(Ok(1_u64), |(sequence, _)| {
        sequence.checked_add(1).ok_or(Error::Limit)
    })?;
    let generation = datastore.join(format!("generation-{next:020}"));
    // An unexplained leftover directory must not be reused or silently erased.
    if generation.exists() {
        return Err(Error::RecoveryRequired);
    }
    publish_small(
        &datastore.join("pending.json"),
        &serde_json::to_vec(&Commit {
            format_version: 1,
            generation: next,
        })
        .map_err(|_| Error::State("cannot encode metadata pointer"))?,
    )?;
    create_private_directory(&generation)?;
    if let Some((_, active)) = active {
        for entry in std::fs::read_dir(active)? {
            let entry = entry?;
            let bytes = read_small(&entry.path())?;
            publish_small(&generation.join(entry.file_name()), &bytes)?;
        }
    }
    Ok((generation, next))
}

fn resume_generation(datastore: &Path) -> Result<(PathBuf, u64)> {
    let pending_path = datastore.join("pending.json");
    if !pending_path.exists() {
        return begin_generation(datastore);
    }
    let bytes = read_small(&pending_path)?;
    strict_json(&bytes).map_err(|_| Error::RecoveryRequired)?;
    let pending: Commit = serde_json::from_slice(&bytes).map_err(|_| Error::RecoveryRequired)?;
    if pending.format_version != 1 || pending.generation == 0 {
        return Err(Error::RecoveryRequired);
    }
    let generation = datastore.join(format!("generation-{:020}", pending.generation));
    validate_generation(&generation).map_err(|_| Error::RecoveryRequired)?;
    let commit_path = datastore.join(format!("commit-{:020}.json", pending.generation));
    if commit_path.exists() {
        let bytes = read_small(&commit_path)?;
        strict_json(&bytes).map_err(|_| Error::RecoveryRequired)?;
        let committed: Commit =
            serde_json::from_slice(&bytes).map_err(|_| Error::RecoveryRequired)?;
        if committed.format_version != 1 || committed.generation != pending.generation {
            return Err(Error::RecoveryRequired);
        }
        // Publication already succeeded; never mutate that committed generation.
        std::fs::remove_file(pending_path)?;
        sync_directory(datastore)?;
        begin_generation(datastore)
    } else {
        Ok((generation, pending.generation))
    }
}

fn commit_generation(datastore: &Path, generation: &Path, sequence: u64) -> Result<()> {
    validate_generation(generation)?;
    sync_generation(generation)?;
    let commit = serde_json::to_vec(&Commit {
        format_version: 1,
        generation: sequence,
    })
    .map_err(|_| Error::State("cannot encode metadata pointer"))?;
    publish_small(
        &datastore.join(format!("commit-{sequence:020}.json")),
        &commit,
    )?;
    std::fs::remove_file(datastore.join("pending.json"))?;
    sync_directory(datastore)?;
    collect_generations(datastore, sequence)?;
    Ok(())
}

fn collect_generations(datastore: &Path, newest: u64) -> Result<()> {
    // Keep the active generation and its predecessor. Neither is ever a fallback:
    // a malformed active pointer remains an error requiring explicit recovery.
    for entry in std::fs::read_dir(datastore)? {
        let entry = entry?;
        let name = entry
            .file_name()
            .into_string()
            .map_err(|_| Error::State("invalid metadata state entry"))?;
        let Some(sequence) = name
            .strip_prefix("commit-")
            .and_then(|name| name.strip_suffix(".json"))
        else {
            continue;
        };
        let sequence = sequence
            .parse::<u64>()
            .map_err(|_| Error::State("invalid metadata generation"))?;
        if sequence >= newest.saturating_sub(1) {
            continue;
        }
        ensure_regular_path(&entry.path())?;
        let generation = datastore.join(format!("generation-{sequence:020}"));
        if !generation.starts_with(datastore) {
            return Err(Error::Ownership);
        }
        validate_generation(&generation)?;
        // Remove the obsolete pointer first. A crash cannot expose a missing active
        // generation; an orphan obsolete directory is harmless retained trust data.
        std::fs::remove_file(entry.path())?;
        for state in std::fs::read_dir(&generation)? {
            let state = state?;
            ensure_regular_path(&state.path())?;
            std::fs::remove_file(state.path())?;
        }
        std::fs::remove_dir(&generation)?;
    }
    sync_directory(datastore)
}

fn validate_generation(generation: &Path) -> Result<()> {
    ensure_directory(generation)?;
    let mut count = 0_usize;
    for entry in std::fs::read_dir(generation)? {
        count = count.checked_add(1).ok_or(Error::Limit)?;
        if count > MAX_DATASTORE_FILES {
            return Err(Error::Limit);
        }
        let entry = entry?;
        let name = entry
            .file_name()
            .into_string()
            .map_err(|_| Error::State("invalid metadata state file"))?;
        if !matches!(
            name.as_str(),
            "root.json"
                | "timestamp.json"
                | "snapshot.json"
                | "targets.json"
                | "latest_known_time.json"
        ) {
            return Err(Error::State("unsupported metadata state file"));
        }
        let bytes = read_small(&entry.path())?;
        strict_json(&bytes).map_err(|_| Error::State("corrupt persisted trust state"))?;
        let valid =
            match name.as_str() {
                "root.json" => validate_root(&bytes).is_ok(),
                "timestamp.json" => serde_json::from_slice::<
                    tough::schema::Signed<tough::schema::Timestamp>,
                >(&bytes)
                .is_ok(),
                "snapshot.json" => {
                    serde_json::from_slice::<tough::schema::Signed<tough::schema::Snapshot>>(&bytes)
                        .is_ok()
                }
                "targets.json" => {
                    serde_json::from_slice::<tough::schema::Signed<tough::schema::Targets>>(&bytes)
                        .is_ok()
                }
                "latest_known_time.json" => serde_json::from_slice::<String>(&bytes)
                    .ok()
                    .is_some_and(|value| {
                        time::OffsetDateTime::parse(
                            &value,
                            &time::format_description::well_known::Rfc3339,
                        )
                        .is_ok()
                    }),
                _ => false,
            };
        if !valid {
            return Err(Error::State("corrupt persisted trust role or clock"));
        }
    }
    Ok(())
}

fn publish_small(path: &Path, bytes: &[u8]) -> Result<()> {
    let temporary = path.with_extension("partial");
    let mut file = OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(&temporary)?;
    file.write_all(bytes)?;
    file.sync_all()?;
    drop(file);
    if path.exists() {
        return Err(Error::RecoveryRequired);
    }
    std::fs::rename(temporary, path)?;
    sync_directory(
        path.parent()
            .ok_or(Error::State("metadata path has no parent"))?,
    )
}

fn read_small(path: &Path) -> Result<Vec<u8>> {
    ensure_regular_path(path)?;
    let file = File::open(path)?;
    if file.metadata()?.len() > METADATA_BYTES {
        return Err(Error::Limit);
    }
    let mut bytes = Vec::new();
    file.take(METADATA_BYTES + 1).read_to_end(&mut bytes)?;
    if u64::try_from(bytes.len()).map_err(|_| Error::Limit)? > METADATA_BYTES {
        return Err(Error::Limit);
    }
    Ok(bytes)
}

pub(crate) fn ensure_regular_path(path: &Path) -> Result<()> {
    check_ancestors(path)?;
    if !std::fs::symlink_metadata(path)?.is_file() {
        return Err(Error::Ownership);
    }
    Ok(())
}

pub(crate) fn ensure_directory(path: &Path) -> Result<()> {
    check_ancestors(path)?;
    if !std::fs::symlink_metadata(path)?.is_dir() {
        return Err(Error::Ownership);
    }
    Ok(())
}

pub(crate) fn create_private_directory(path: &Path) -> Result<()> {
    if path.exists() {
        return ensure_directory(path);
    }
    let parent = path.parent().ok_or(Error::Ownership)?;
    ensure_directory(parent)?;
    let builder = std::fs::DirBuilder::new();
    #[cfg(unix)]
    let builder = {
        use std::os::unix::fs::DirBuilderExt;
        let mut builder = builder;
        builder.mode(0o700);
        builder
    };
    builder.create(path)?;
    ensure_directory(path)?;
    sync_directory(parent)
}

fn check_ancestors(path: &Path) -> Result<()> {
    if !path.is_absolute() {
        return Err(Error::Ownership);
    }
    for ancestor in path.ancestors() {
        let metadata = std::fs::symlink_metadata(ancestor)?;
        if metadata.file_type().is_symlink() || is_reparse(&metadata) {
            return Err(Error::Ownership);
        }
    }
    Ok(())
}

#[cfg(windows)]
fn is_reparse(metadata: &std::fs::Metadata) -> bool {
    use std::os::windows::fs::MetadataExt;
    metadata.file_attributes() & 0x400 != 0
}

#[cfg(not(windows))]
fn is_reparse(_metadata: &std::fs::Metadata) -> bool {
    false
}

fn sync_generation(generation: &Path) -> Result<()> {
    validate_generation(generation)?;
    for entry in std::fs::read_dir(generation)? {
        OpenOptions::new()
            .read(true)
            .write(true)
            .open(entry?.path())?
            .sync_all()?;
    }
    sync_directory(generation)
}

#[cfg(unix)]
pub(crate) fn sync_directory(path: &Path) -> Result<()> {
    File::open(path)?.sync_all().map_err(Error::Io)
}

#[cfg(not(unix))]
pub(crate) fn sync_directory(_path: &Path) -> Result<()> {
    // Windows file publication is synced before same-volume rename. Directory flush
    // requires a platform helper; interrupted markers fail closed on the next open.
    Ok(())
}

fn hex_digest(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    let mut result = String::with_capacity(bytes.len().saturating_mul(2));
    for byte in bytes {
        // Writing to a String cannot fail; avoid a panic even if that contract changes.
        if write!(&mut result, "{byte:02x}").is_err() {
            return String::new();
        }
    }
    result
}

// serde_json::Value silently replaces duplicate keys. Reject them recursively before
// Tough canonicalizes signatures or Knowell interprets signed custom metadata.
struct UniqueJson;

impl<'de> DeserializeSeed<'de> for UniqueJson {
    type Value = ();
    fn deserialize<D: serde::Deserializer<'de>>(
        self,
        deserializer: D,
    ) -> std::result::Result<(), D::Error> {
        deserializer.deserialize_any(self)
    }
}

impl<'de> Visitor<'de> for UniqueJson {
    type Value = ();
    fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("json without duplicate keys")
    }
    fn visit_bool<E: serde::de::Error>(self, _value: bool) -> std::result::Result<(), E> {
        Ok(())
    }
    fn visit_i64<E: serde::de::Error>(self, _value: i64) -> std::result::Result<(), E> {
        Ok(())
    }
    fn visit_u64<E: serde::de::Error>(self, _value: u64) -> std::result::Result<(), E> {
        Ok(())
    }
    fn visit_f64<E: serde::de::Error>(self, _value: f64) -> std::result::Result<(), E> {
        Ok(())
    }
    fn visit_str<E: serde::de::Error>(self, _value: &str) -> std::result::Result<(), E> {
        Ok(())
    }
    fn visit_string<E: serde::de::Error>(self, _value: String) -> std::result::Result<(), E> {
        Ok(())
    }
    fn visit_unit<E: serde::de::Error>(self) -> std::result::Result<(), E> {
        Ok(())
    }
    fn visit_none<E: serde::de::Error>(self) -> std::result::Result<(), E> {
        Ok(())
    }
    fn visit_seq<A: SeqAccess<'de>>(self, mut sequence: A) -> std::result::Result<(), A::Error> {
        while sequence.next_element_seed(UniqueJson)?.is_some() {}
        Ok(())
    }
    fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> std::result::Result<(), A::Error> {
        let mut keys = BTreeSet::new();
        while let Some(key) = map.next_key::<String>()? {
            if !keys.insert(key) {
                return Err(serde::de::Error::custom("duplicate json key"));
            }
            map.next_value_seed(UniqueJson)?;
        }
        Ok(())
    }
}

pub(crate) fn strict_json(bytes: &[u8]) -> Result<()> {
    let mut deserializer = serde_json::Deserializer::from_slice(bytes);
    UniqueJson
        .deserialize(&mut deserializer)
        .map_err(|_| Error::Manifest)?;
    deserializer.end().map_err(|_| Error::Manifest)
}

pub(crate) fn validate_root(bytes: &[u8]) -> Result<()> {
    strict_json(bytes)?;
    let root: tough::schema::Signed<tough::schema::Root> =
        serde_json::from_slice(bytes).map_err(|_| Error::Metadata)?;
    if root.signed.version.get() > MAX_ROOT_VERSION {
        return Err(Error::Limit);
    }
    root.signed
        .verify_role(&root)
        .map_err(|_| Error::Metadata)?;
    Ok(())
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use std::num::NonZeroU64;
    use tough::editor::RepositoryEditor;
    use tough::editor::signed::SignedRole;
    use tough::key_source::{KeySource, LocalKeySource};
    use tough::schema::{KeyHolder, RoleKeys, RoleType, Root, Signed, Target, Targets};

    struct Fixture {
        _temporary: tempfile::TempDir,
        config: RepositoryConfig,
        key_path: PathBuf,
        root_path: PathBuf,
        engine_path: PathBuf,
        engine: Artifact,
    }

    impl Fixture {
        async fn new() -> Self {
            let temporary = tempfile::tempdir().unwrap();
            let base = temporary.path().canonicalize().unwrap();
            let key_path = base.join("synthetic-signing-key.der");
            // Tough expects Ed25519 PKCS#8 DER. This ephemeral test-only key never
            // becomes an application trust anchor or leaves the temporary fixture.
            let key = aws_lc_rs::signature::Ed25519KeyPair::generate_pkcs8(
                &aws_lc_rs::rand::SystemRandom::new(),
            )
            .unwrap();
            std::fs::write(&key_path, key.as_ref()).unwrap();
            let keys: Vec<Box<dyn KeySource>> = vec![Box::new(LocalKeySource {
                path: key_path.clone(),
            })];
            let key = keys.first().unwrap().as_sign().await.unwrap().tuf_key();
            let key_id = key.key_id().unwrap();
            let role = RoleKeys {
                keyids: vec![key_id.clone()],
                threshold: NonZeroU64::new(1).unwrap(),
                _extra: HashMap::new(),
            };
            let root = Root {
                spec_version: "1.0.0".to_owned(),
                consistent_snapshot: true,
                version: NonZeroU64::new(1).unwrap(),
                expires: "2100-01-01T00:00:00Z".parse().unwrap(),
                keys: HashMap::from([(key_id, key)]),
                roles: [
                    RoleType::Root,
                    RoleType::Timestamp,
                    RoleType::Snapshot,
                    RoleType::Targets,
                ]
                .into_iter()
                .map(|kind| (kind, role.clone()))
                .collect(),
                _extra: HashMap::new(),
            };
            let signed_root = SignedRole::new(
                root.clone(),
                &KeyHolder::Root(root),
                &keys,
                &aws_lc_rs::rand::SystemRandom::new(),
            )
            .await
            .unwrap();
            let root_path = base.join("root.json");
            std::fs::write(&root_path, signed_root.buffer()).unwrap();
            let metadata_path = base.join("metadata");
            let targets_path = base.join("targets");
            std::fs::create_dir(&metadata_path).unwrap();
            std::fs::create_dir(&targets_path).unwrap();
            let config = RepositoryConfig {
                trusted_root: Some(signed_root.buffer().to_vec()),
                metadata_url: Url::from_directory_path(&metadata_path).unwrap(),
                targets_url: Url::from_directory_path(&targets_path).unwrap(),
                datastore: base.join("datastore"),
                offline: true,
                recover_pending: false,
                max_target_bytes: 1024 * 1024,
            };
            let version = semver::Version::parse("1.0.0").unwrap();
            let target = "x86_64-unknown-linux-gnu";
            let engine_name =
                crate::manifest::target_name(&version, target, crate::manifest::Component::Engine)
                    .unwrap();
            let launcher_name = crate::manifest::target_name(
                &version,
                target,
                crate::manifest::Component::Launcher,
            )
            .unwrap();
            let engine = Artifact {
                name: engine_name,
                sha256: hex_digest(&Sha256::digest(b"synthetic engine")),
                size: 16,
            };
            let engine_path = physical_target_path(&targets_path, &engine);
            std::fs::create_dir(engine_path.parent().unwrap()).unwrap();
            std::fs::write(&engine_path, b"synthetic engine").unwrap();
            let launcher = Artifact {
                name: launcher_name,
                sha256: hex_digest(&Sha256::digest(b"synthetic launcher")),
                size: 18,
            };
            std::fs::write(
                physical_target_path(&targets_path, &launcher),
                b"synthetic launcher",
            )
            .unwrap();
            let fixture = Self {
                _temporary: temporary,
                config,
                key_path,
                root_path,
                engine_path,
                engine,
            };
            fixture.publish(1, "2100-01-01T00:00:00Z", launcher).await;
            fixture
        }

        async fn publish(&self, version: u64, expiration: &str, launcher: Artifact) {
            self.publish_state(version, expiration, launcher, false)
                .await;
        }

        async fn publish_state(
            &self,
            version: u64,
            expiration: &str,
            launcher: Artifact,
            revoked: bool,
        ) {
            self.publish_contract(version, expiration, launcher, revoked, 1)
                .await;
        }

        async fn publish_contract(
            &self,
            version: u64,
            expiration: &str,
            launcher: Artifact,
            revoked: bool,
            config_max: u32,
        ) {
            let keys: Vec<Box<dyn KeySource>> = vec![Box::new(LocalKeySource {
                path: self.key_path.clone(),
            })];
            let mut editor = RepositoryEditor::new(&self.root_path).await.unwrap();
            let version = NonZeroU64::new(version).unwrap();
            // Tough's bare editor emits an empty delegation block by default. This
            // fixture deliberately exercises our non-delegated manifest format.
            editor
                .targets(Signed {
                    signed: Targets {
                        spec_version: "1.0.0".to_owned(),
                        version,
                        expires: "2100-01-01T00:00:00Z".parse().unwrap(),
                        targets: HashMap::new(),
                        delegations: None,
                        _extra: HashMap::new(),
                    },
                    signatures: Vec::new(),
                })
                .unwrap();
            editor
                .targets_version(version)
                .unwrap()
                .targets_expires("2100-01-01T00:00:00Z".parse().unwrap())
                .unwrap()
                .snapshot_version(version)
                .snapshot_expires("2100-01-01T00:00:00Z".parse().unwrap())
                .timestamp_version(version)
                .timestamp_expires(expiration.parse().unwrap());
            let mut engine_target = Target::from_path(&self.engine_path).await.unwrap();
            engine_target.custom.insert("knowell".to_owned(), serde_json::json!({
                "format_version": 1,
                "version": "1.0.0",
                "target": "x86_64-unknown-linux-gnu",
                "channel": "stable",
                "component": "engine",
                "revoked": revoked,
                "launcher": launcher,
                "compatibility": {
                    "schema": {"read_min":1,"read_max":1,"write_min":1,"write_max":1},
                    "config":{"min":1,"max":config_max},"index":{"min":1,"max":1},
                    "jobs":{"min":1,"max":1},"protocol":{"min":1,"max":1},"launcher":{"min":1,"max":1}
                }
            }));
            let launcher_path =
                physical_target_path(&self.config.targets_url.to_file_path().unwrap(), &launcher);
            editor
                .add_target(
                    tough::TargetName::new(&self.engine.name).unwrap(),
                    engine_target,
                )
                .unwrap()
                .add_target(
                    tough::TargetName::new(&launcher.name).unwrap(),
                    Target::from_path(launcher_path).await.unwrap(),
                )
                .unwrap();
            let signed = editor.sign(&keys).await.unwrap();
            signed
                .write(self.config.metadata_url.to_file_path().unwrap())
                .await
                .unwrap();
        }

        async fn rotate_root(&mut self, cross_sign: bool) {
            let previous: Signed<Root> =
                serde_json::from_slice(&std::fs::read(&self.root_path).unwrap()).unwrap();
            let new_key_path = self._temporary.path().join("synthetic-rotated-key.der");
            let key = aws_lc_rs::signature::Ed25519KeyPair::generate_pkcs8(
                &aws_lc_rs::rand::SystemRandom::new(),
            )
            .unwrap();
            std::fs::write(&new_key_path, key.as_ref()).unwrap();
            let old_keys: Vec<Box<dyn KeySource>> = vec![Box::new(LocalKeySource {
                path: self.key_path.clone(),
            })];
            let new_keys: Vec<Box<dyn KeySource>> = vec![Box::new(LocalKeySource {
                path: new_key_path.clone(),
            })];
            let public = new_keys.first().unwrap().as_sign().await.unwrap().tuf_key();
            let key_id = public.key_id().unwrap();
            let role = RoleKeys {
                keyids: vec![key_id.clone()],
                threshold: NonZeroU64::new(1).unwrap(),
                _extra: HashMap::new(),
            };
            let mut root = previous.signed.clone();
            root.version = NonZeroU64::new(root.version.get() + 1).unwrap();
            root.keys = HashMap::from([(key_id, public)]);
            root.roles = [
                RoleType::Root,
                RoleType::Timestamp,
                RoleType::Snapshot,
                RoleType::Targets,
            ]
            .into_iter()
            .map(|kind| (kind, role.clone()))
            .collect();
            let mut signed = SignedRole::new(
                root.clone(),
                &KeyHolder::Root(root.clone()),
                &new_keys,
                &aws_lc_rs::rand::SystemRandom::new(),
            )
            .await
            .unwrap();
            if cross_sign {
                let old_signatures = SignedRole::new(
                    root,
                    &KeyHolder::Root(previous.signed),
                    &old_keys,
                    &aws_lc_rs::rand::SystemRandom::new(),
                )
                .await
                .unwrap();
                signed = signed
                    .add_old_signatures(old_signatures.signed().signatures.clone())
                    .unwrap();
            }
            signed
                .write(self.config.metadata_url.to_file_path().unwrap(), true)
                .await
                .unwrap();
            std::fs::write(&self.root_path, signed.buffer()).unwrap();
            self.key_path = new_key_path;
        }
    }

    fn physical_target_path(base: &Path, artifact: &Artifact) -> PathBuf {
        let (version, basename) = artifact.name.split_once('/').unwrap();
        base.join(version)
            .join(format!("{}.{basename}", artifact.sha256))
    }

    #[tokio::test]
    async fn real_tuf_signatures_paired_launcher_and_exact_stream_are_verified() {
        let fixture = Fixture::new().await;
        let repository = TrustedRepository::load(fixture.config.clone())
            .await
            .unwrap();
        assert_eq!(repository.releases().len(), 1);
        let target = repository.releases().first().unwrap();
        assert_eq!(target.artifact, fixture.engine);
        let staging_path = fixture._temporary.path().join("stage");
        let mut staging = tokio::fs::OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(&staging_path)
            .await
            .unwrap();
        repository
            .download(&fixture.engine, &mut staging)
            .await
            .unwrap();
        assert_eq!(std::fs::read(staging_path).unwrap(), b"synthetic engine");
        assert!(!fixture.config.datastore.join("pending.json").exists());
        assert!(
            fixture
                .config
                .datastore
                .join("commit-00000000000000000002.json")
                .is_file()
        );
    }

    #[tokio::test]
    async fn truncated_target_and_tampered_signed_metadata_fail_without_activation() {
        let fixture = Fixture::new().await;
        let repository = TrustedRepository::load(fixture.config.clone())
            .await
            .unwrap();
        std::fs::write(&fixture.engine_path, b"truncated").unwrap();
        let mut staging = tokio::fs::OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(fixture._temporary.path().join("stage"))
            .await
            .unwrap();
        assert!(matches!(
            repository.download(&fixture.engine, &mut staging).await,
            Err(Error::Integrity)
        ));
        drop(repository);
        let timestamp_path = fixture
            .config
            .metadata_url
            .to_file_path()
            .unwrap()
            .join("timestamp.json");
        let mut timestamp: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&timestamp_path).unwrap()).unwrap();
        timestamp
            .get_mut("signed")
            .unwrap()
            .as_object_mut()
            .unwrap()
            .insert("version".to_owned(), 999.into());
        std::fs::write(timestamp_path, serde_json::to_vec(&timestamp).unwrap()).unwrap();
        assert!(matches!(
            TrustedRepository::load(fixture.config.clone()).await,
            Err(Error::Metadata)
        ));
    }

    #[tokio::test]
    async fn rollback_expiry_and_source_switch_preserve_persistent_high_water() {
        let fixture = Fixture::new().await;
        let first = TrustedRepository::load(fixture.config.clone())
            .await
            .unwrap();
        let launcher = first.releases().first().unwrap().metadata.launcher.clone();
        drop(first);
        fixture
            .publish(2, "2100-01-01T00:00:00Z", launcher.clone())
            .await;
        drop(
            TrustedRepository::load(fixture.config.clone())
                .await
                .unwrap(),
        );
        fixture
            .publish(1, "2100-01-01T00:00:00Z", launcher.clone())
            .await;
        assert!(matches!(
            TrustedRepository::load(fixture.config.clone()).await,
            Err(Error::Metadata)
        ));
        fixture.publish(3, "2000-01-01T00:00:00Z", launcher).await;
        assert!(matches!(
            TrustedRepository::load(fixture.config.clone()).await,
            Err(Error::Metadata)
        ));
        let mut different = fixture.config.clone();
        different.targets_url = different.metadata_url.clone();
        assert!(matches!(
            TrustedRepository::load(different).await,
            Err(Error::State(_))
        ));
        let commits = std::fs::read_dir(&fixture.config.datastore)
            .unwrap()
            .filter(|entry| {
                entry
                    .as_ref()
                    .unwrap()
                    .file_name()
                    .to_string_lossy()
                    .starts_with("commit-")
            })
            .count();
        assert_eq!(commits, 2);
    }

    #[tokio::test]
    async fn root_key_rotation_survives_later_role_failure_and_lost_intermediate_root() {
        let mut fixture = Fixture::new().await;
        let first = TrustedRepository::load(fixture.config.clone())
            .await
            .unwrap();
        let launcher = first.releases().first().unwrap().metadata.launcher.clone();
        drop(first);
        fixture.rotate_root(true).await;
        fixture
            .publish(2, "2100-01-01T00:00:00Z", launcher.clone())
            .await;
        let metadata = fixture.config.metadata_url.to_file_path().unwrap();
        let timestamp_path = metadata.join("timestamp.json");
        let mut timestamp: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&timestamp_path).unwrap()).unwrap();
        timestamp
            .get_mut("signed")
            .unwrap()
            .as_object_mut()
            .unwrap()
            .insert("version".to_owned(), 999.into());
        std::fs::write(timestamp_path, serde_json::to_vec(&timestamp).unwrap()).unwrap();
        assert!(matches!(
            TrustedRepository::load(fixture.config.clone()).await,
            Err(Error::Metadata)
        ));
        let persisted: Signed<Root> = serde_json::from_slice(
            &std::fs::read(
                fixture
                    .config
                    .datastore
                    .join("generation-00000000000000000002")
                    .join("root.json"),
            )
            .unwrap(),
        )
        .unwrap();
        assert_eq!(persisted.signed.version.get(), 2);
        fixture.publish(2, "2100-01-01T00:00:00Z", launcher).await;
        std::fs::remove_file(metadata.join("2.root.json")).unwrap();
        let recovered = TrustedRepository::load(fixture.config.clone())
            .await
            .unwrap();
        assert_eq!(recovered.releases().len(), 1);
    }

    #[tokio::test]
    async fn root_rotation_requires_old_and_new_threshold_signatures() {
        let mut fixture = Fixture::new().await;
        let first = TrustedRepository::load(fixture.config.clone())
            .await
            .unwrap();
        let launcher = first.releases().first().unwrap().metadata.launcher.clone();
        drop(first);
        fixture.rotate_root(false).await;
        fixture.publish(2, "2100-01-01T00:00:00Z", launcher).await;
        assert!(matches!(
            TrustedRepository::load(fixture.config.clone()).await,
            Err(Error::Metadata)
        ));
        let persisted: Signed<Root> = serde_json::from_slice(
            &std::fs::read(
                fixture
                    .config
                    .datastore
                    .join("generation-00000000000000000002")
                    .join("root.json"),
            )
            .unwrap(),
        )
        .unwrap();
        assert_eq!(persisted.signed.version.get(), 1);
    }

    #[tokio::test]
    async fn signed_revocation_between_plan_and_download_blocks_both_executables() {
        let fixture = Fixture::new().await;
        let repository = TrustedRepository::load(fixture.config.clone())
            .await
            .unwrap();
        let launcher = repository
            .releases()
            .first()
            .unwrap()
            .metadata
            .launcher
            .clone();
        fixture
            .publish_state(2, "2100-01-01T00:00:00Z", launcher.clone(), true)
            .await;
        for (name, artifact) in [
            ("engine-stage", &fixture.engine),
            ("launcher-stage", &launcher),
        ] {
            let path = fixture._temporary.path().join(name);
            let mut staging = tokio::fs::OpenOptions::new()
                .create_new(true)
                .write(true)
                .open(&path)
                .await
                .unwrap();
            assert!(matches!(
                repository.download(artifact, &mut staging).await,
                Err(Error::Revoked)
            ));
            assert_eq!(std::fs::metadata(path).unwrap().len(), 0);
        }
        drop(repository);
        fixture.publish(1, "2100-01-01T00:00:00Z", launcher).await;
        assert!(matches!(
            TrustedRepository::load(fixture.config.clone()).await,
            Err(Error::Metadata)
        ));
    }

    #[tokio::test]
    async fn signed_same_engine_bytes_cannot_change_planned_compatibility_or_launcher() {
        let fixture = Fixture::new().await;
        let repository = TrustedRepository::load(fixture.config.clone())
            .await
            .unwrap();
        let launcher = repository
            .releases()
            .first()
            .unwrap()
            .metadata
            .launcher
            .clone();
        fixture
            .publish_contract(2, "2100-01-01T00:00:00Z", launcher.clone(), false, 2)
            .await;
        let first_path = fixture._temporary.path().join("compatibility-stage");
        let mut first = tokio::fs::OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(&first_path)
            .await
            .unwrap();
        assert!(matches!(
            repository.download(&fixture.engine, &mut first).await,
            Err(Error::Integrity)
        ));
        assert_eq!(std::fs::metadata(first_path).unwrap().len(), 0);
        let replacement = b"synthetic changed launcher";
        let changed = Artifact {
            name: launcher.name.clone(),
            sha256: hex_digest(&Sha256::digest(replacement)),
            size: u64::try_from(replacement.len()).unwrap(),
        };
        std::fs::write(
            physical_target_path(
                &fixture.config.targets_url.to_file_path().unwrap(),
                &changed,
            ),
            replacement,
        )
        .unwrap();
        fixture
            .publish_state(3, "2100-01-01T00:00:00Z", changed, false)
            .await;
        for (name, artifact) in [
            ("changed-engine-stage", &fixture.engine),
            ("old-launcher-stage", &launcher),
        ] {
            let path = fixture._temporary.path().join(name);
            let mut staging = tokio::fs::OpenOptions::new()
                .create_new(true)
                .write(true)
                .open(&path)
                .await
                .unwrap();
            assert!(matches!(
                repository.download(artifact, &mut staging).await,
                Err(Error::Integrity)
            ));
            assert_eq!(std::fs::metadata(path).unwrap().len(), 0);
        }
    }

    #[tokio::test]
    async fn invalid_bootstrap_and_overflow_root_versions_fail_before_pinning() {
        let fixture = Fixture::new().await;
        let mut root: Signed<Root> =
            serde_json::from_slice(fixture.config.trusted_root.as_ref().unwrap()).unwrap();
        root.signatures.clear();
        let mut invalid = fixture.config.clone();
        invalid.trusted_root = Some(serde_json::to_vec(&root).unwrap());
        assert!(matches!(
            TrustedRepository::load(invalid).await,
            Err(Error::Metadata)
        ));
        assert!(!fixture.config.datastore.exists());
        root.signed.version = NonZeroU64::new(u64::MAX).unwrap();
        let mut overflow = fixture.config.clone();
        overflow.trusted_root = Some(serde_json::to_vec(&root).unwrap());
        assert!(matches!(
            TrustedRepository::load(overflow).await,
            Err(Error::Limit)
        ));
        assert!(!fixture.config.datastore.exists());
    }

    #[tokio::test]
    async fn explicit_configuration_corrects_endpoint_typo_without_changing_bootstrap() {
        let fixture = Fixture::new().await;
        let wrong_directory = fixture
            .config
            .datastore
            .parent()
            .unwrap()
            .join("wrong-metadata");
        std::fs::create_dir(&wrong_directory).unwrap();
        let mut wrong = fixture.config.clone();
        wrong.metadata_url = Url::from_directory_path(wrong_directory).unwrap();
        assert!(matches!(
            TrustedRepository::load(wrong).await,
            Err(Error::Metadata)
        ));
        assert!(matches!(
            TrustedRepository::load(fixture.config.clone()).await,
            Err(Error::State(_))
        ));
        let configured = TrustedRepository::configure(fixture.config.clone())
            .await
            .unwrap();
        assert_eq!(configured.releases().len(), 1);
        drop(configured);
        assert!(
            TrustedRepository::load(fixture.config.clone())
                .await
                .is_ok()
        );
    }

    #[tokio::test]
    async fn failed_endpoint_relocation_preserves_accepted_high_water_and_old_source_pin() {
        let fixture = Fixture::new().await;
        let first = TrustedRepository::load(fixture.config.clone())
            .await
            .unwrap();
        let launcher = first.releases().first().unwrap().metadata.launcher.clone();
        drop(first);
        let source_path = fixture.config.datastore.join("source.json");
        let original_source = std::fs::read(&source_path).unwrap();
        fixture
            .publish(2, "2100-01-01T00:00:00Z", launcher.clone())
            .await;
        let original_metadata = fixture.config.metadata_url.to_file_path().unwrap();
        let relocated_metadata = fixture
            .config
            .datastore
            .parent()
            .unwrap()
            .join("relocated-metadata");
        std::fs::create_dir(&relocated_metadata).unwrap();
        for entry in std::fs::read_dir(&original_metadata).unwrap() {
            let entry = entry.unwrap();
            std::fs::copy(entry.path(), relocated_metadata.join(entry.file_name())).unwrap();
        }
        let snapshot_path = relocated_metadata.join("2.snapshot.json");
        let valid_snapshot = std::fs::read(&snapshot_path).unwrap();
        std::fs::write(&snapshot_path, b"truncated").unwrap();
        let mut relocated = fixture.config.clone();
        relocated.metadata_url = Url::from_directory_path(&relocated_metadata).unwrap();
        assert!(matches!(
            TrustedRepository::configure(relocated.clone()).await,
            Err(Error::Metadata)
        ));
        assert_eq!(std::fs::read(&source_path).unwrap(), original_source);
        let persisted: Signed<tough::schema::Timestamp> = serde_json::from_slice(
            &std::fs::read(
                fixture
                    .config
                    .datastore
                    .join("generation-00000000000000000002")
                    .join("timestamp.json"),
            )
            .unwrap(),
        )
        .unwrap();
        assert_eq!(persisted.signed.version.get(), 2);
        fixture.publish(1, "2100-01-01T00:00:00Z", launcher).await;
        assert!(matches!(
            TrustedRepository::load(fixture.config.clone()).await,
            Err(Error::Metadata)
        ));
        std::fs::write(snapshot_path, valid_snapshot).unwrap();
        drop(
            TrustedRepository::configure(relocated.clone())
                .await
                .unwrap(),
        );
        assert!(TrustedRepository::load(relocated).await.is_ok());
        assert!(matches!(
            TrustedRepository::load(fixture.config.clone()).await,
            Err(Error::State(_))
        ));
    }

    #[tokio::test]
    async fn endpoint_configuration_cannot_replace_the_original_bootstrap_anchor() {
        let fixture = Fixture::new().await;
        drop(
            TrustedRepository::load(fixture.config.clone())
                .await
                .unwrap(),
        );
        let other = Fixture::new().await;
        let mut changed = fixture.config.clone();
        changed.trusted_root = other.config.trusted_root.clone();
        assert!(matches!(
            TrustedRepository::configure(changed).await,
            Err(Error::State(_))
        ));
        assert!(
            TrustedRepository::load(fixture.config.clone())
                .await
                .is_ok()
        );
    }

    #[test]
    fn explicit_recovery_resumes_only_the_pending_generation() {
        let root = tempfile::tempdir().unwrap();
        let (generation, sequence) = begin_generation(root.path()).unwrap();
        publish_small(
            &generation.join("latest_known_time.json"),
            br#""2020-01-01T00:00:00Z""#,
        )
        .unwrap();
        assert!(matches!(
            begin_generation(root.path()),
            Err(Error::RecoveryRequired)
        ));
        let resumed = resume_generation(root.path()).unwrap();
        assert_eq!(resumed, (generation.clone(), sequence));
        std::fs::write(generation.join("latest_known_time.json"), b"truncated").unwrap();
        assert!(matches!(
            resume_generation(root.path()),
            Err(Error::RecoveryRequired)
        ));
    }

    #[test]
    fn duplicate_truncated_and_hostile_json_are_rejected() {
        for input in [
            b"{\"x\":1,\"x\":2}".as_slice(),
            b"{\"custom\":{\"x\":1,\"x\":2}}",
            b"{",
            b"[] true",
        ] {
            assert!(strict_json(input).is_err());
        }
        assert!(strict_json(br#"{"custom":{"x":[null,true,1,"ok"]}}"#).is_ok());
    }

    #[test]
    fn unconfigured_trust_and_sensitive_urls_fail_without_echoing() {
        let rejected =
            Url::parse("https://FAKE_USER:KNOWELL_CANARY_FAKE_PASSWORD@example.invalid/metadata/")
                .unwrap();
        let error = validate_base(&rejected, false).unwrap_err();
        assert!(!format!("{error:?} {error}").contains("KNOWELL_CANARY"));
        assert!(matches!(
            validate_base(&Url::parse("http://example.invalid/").unwrap(), false),
            Err(Error::Source)
        ));
        assert!(matches!(
            validate_base(&Url::parse("file://remote.invalid/share/").unwrap(), true),
            Err(Error::Source)
        ));
    }

    #[test]
    fn generation_commit_preserves_active_state_and_detects_pending_recovery() {
        let root = tempfile::tempdir().unwrap();
        let (generation, sequence) = begin_generation(root.path()).unwrap();
        publish_small(
            &generation.join("latest_known_time.json"),
            br#""2020-01-01T00:00:00Z""#,
        )
        .unwrap();
        commit_generation(root.path(), &generation, sequence).unwrap();
        let (next, next_sequence) = begin_generation(root.path()).unwrap();
        assert_eq!(next_sequence, 2);
        assert_eq!(
            read_small(&next.join("latest_known_time.json")).unwrap(),
            br#""2020-01-01T00:00:00Z""#
        );
        assert!(matches!(
            begin_generation(root.path()),
            Err(Error::RecoveryRequired)
        ));
        assert!(
            root.path()
                .join("commit-00000000000000000001.json")
                .is_file()
        );
    }

    #[test]
    fn corrupt_high_water_clock_is_never_reset() {
        let root = tempfile::tempdir().unwrap();
        let (generation, sequence) = begin_generation(root.path()).unwrap();
        publish_small(
            &generation.join("latest_known_time.json"),
            br#""not a timestamp""#,
        )
        .unwrap();
        assert!(commit_generation(root.path(), &generation, sequence).is_err());
        assert!(root.path().join("pending.json").is_file());
        assert!(matches!(
            begin_generation(root.path()),
            Err(Error::RecoveryRequired)
        ));
    }

    #[tokio::test]
    async fn missing_trust_is_a_distinct_failure_before_filesystem_or_network() {
        let config = RepositoryConfig {
            trusted_root: None,
            metadata_url: Url::parse("https://example.invalid/metadata/").unwrap(),
            targets_url: Url::parse("https://example.invalid/targets/").unwrap(),
            datastore: PathBuf::from("does-not-exist"),
            offline: false,
            recover_pending: false,
            max_target_bytes: MAX_TARGET_BYTES,
        };
        assert!(matches!(
            TrustedRepository::load(config).await,
            Err(Error::TrustUnconfigured)
        ));
    }
}
