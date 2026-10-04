//! Offline, threshold-signed metadata publication from completely verified raw assets.
//!
//! Signing keys remain in memory and never enter metadata, logs, or verification caches.
//! Root creation/rotation and deployment to a public repository are separate operator work.

use std::collections::{BTreeMap, BTreeSet};
use std::fs::{File, OpenOptions};
use std::io::Read;
use std::num::NonZeroU64;
use std::path::{Path, PathBuf};

use async_trait::async_trait;
use semver::Version;
use serde::{Deserialize, Serialize};
use tough::editor::RepositoryEditor;
use tough::key_source::KeySource;
use tough::schema::{RoleType, Root, Signed, Target, Targets};

use crate::install::verify_file;
use crate::manifest::{
    Artifact, Channel, Component, MAX_TARGET_BYTES, ReleaseMetadata, ReleaseTarget,
};
use crate::repository::{
    RepositoryConfig, TrustedRepository, create_private_directory, ensure_directory,
    ensure_regular_path, strict_json, sync_directory, validate_root,
};
use crate::{Error, Result};

const MAX_INPUT_BYTES: u64 = 8 * 1024 * 1024;
const MAX_KEY_BYTES: u64 = 64 * 1024;
const MAX_KEYS: usize = 32;
const MAX_TARGETS: usize = 4096;
const REQUIRED_TARGETS: [&str; 6] = [
    "x86_64-unknown-linux-gnu",
    "aarch64-unknown-linux-gnu",
    "x86_64-apple-darwin",
    "aarch64-apple-darwin",
    "x86_64-pc-windows-msvc",
    "aarch64-pc-windows-msvc",
];

/// Explicit offline signing inputs; all paths must be absolute with no linked ancestors.
///
/// Expirations use RFC3339 UTC instants and must be future times. Maximum remaining
/// lifetimes are 365 days for targets, 30 days for snapshot, and 7 days for timestamp.
#[derive(Clone)]
pub struct PublisherConfig {
    /// Existing signed public root, verified against its own threshold.
    pub root: PathBuf,
    /// Strict cumulative `unsigned-update-targets.json` input.
    pub targets: PathBuf,
    /// Offline private key files; each is bounded to 64 KiB and never persisted.
    pub keys: Vec<PathBuf>,
    /// Positive monotonically advanced version for targets, snapshot, and timestamp.
    /// The operator is responsible for advancing the previously published sequence.
    pub sequence: u64,
    /// Targets expiration as an RFC3339 UTC instant.
    pub targets_expires: String,
    /// Snapshot expiration as an RFC3339 UTC instant.
    pub snapshot_expires: String,
    /// Timestamp expiration as an RFC3339 UTC instant.
    pub timestamp_expires: String,
    /// Directory containing every current/historical `<sha256>.<raw-basename>` asset.
    pub assets: PathBuf,
    /// Fresh local metadata output directory; an existing entry is never adopted.
    pub out: PathBuf,
}

impl std::fmt::Debug for PublisherConfig {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("PublisherConfig")
            .field("sequence", &self.sequence)
            .field("key_count", &self.keys.len())
            .finish_non_exhaustive()
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Input {
    format_version: u32,
    version: Version,
    targets: BTreeMap<String, Descriptor>,
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Descriptor {
    length: u64,
    hashes: Hashes,
    custom: BTreeMap<String, serde_json::Value>,
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Hashes {
    sha256: String,
}

struct MemoryKey(Vec<u8>);

impl std::fmt::Debug for MemoryKey {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("offline signing key (redacted)")
    }
}

type KeyResult<T> = std::result::Result<T, Box<dyn std::error::Error + Send + Sync + 'static>>;

#[async_trait]
impl KeySource for MemoryKey {
    async fn as_sign(&self) -> KeyResult<Box<dyn tough::sign::Sign>> {
        Ok(Box::new(tough::sign::parse_keypair(&self.0)?))
    }

    async fn write(&self, _value: &str, _key_id_hex: &str) -> KeyResult<()> {
        Err(Box::new(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            "offline signing sources are read only",
        )))
    }
}

/// Verifies raw assets, signs each mutable role with Tough, self-verifies, then publishes.
///
/// This performs no network calls and never creates production keys. Validation or
/// signing failure leaves output absent. A final I/O failure after the verified
/// metadata rename requires inspecting the output's presence and durability.
/// No cache or private signing material enters output.
pub async fn publish(mut config: PublisherConfig) -> Result<()> {
    config.root = std::path::absolute(&config.root)?;
    config.targets = std::path::absolute(&config.targets)?;
    config.assets = std::path::absolute(&config.assets)?;
    config.out = std::path::absolute(&config.out)?;
    config.keys = config
        .keys
        .iter()
        .map(std::path::absolute)
        .collect::<std::io::Result<_>>()?;
    let root_bytes = read_bounded(&config.root, MAX_INPUT_BYTES)?;
    validate_root(&root_bytes)?;
    validate_public_root(&root_bytes)?;
    let root: Signed<Root> = serde_json::from_slice(&root_bytes).map_err(|_| Error::Metadata)?;
    if !root.signed.consistent_snapshot {
        return Err(Error::State("publisher requires consistent snapshots"));
    }
    validate_expirations(&config, &root)?;
    let sequence = NonZeroU64::new(config.sequence)
        .ok_or(Error::State("publisher sequence must be positive"))?;
    let input_bytes = read_bounded(&config.targets, MAX_INPUT_BYTES)?;
    strict_json(&input_bytes)?;
    let input: Input = serde_json::from_slice(&input_bytes).map_err(|_| Error::Manifest)?;
    let planned = validate_input(&input, &config.assets)?;
    let keys = read_keys(&config.keys, &root).await?;
    let parent = config.out.parent().ok_or(Error::Ownership)?;
    ensure_directory(parent)?;
    reject_existing(&config.out)?;
    let staging = OutputStage::new(parent)?;
    let mirror = staging.path.join("targets");
    create_private_directory(&mirror)?;
    for release in &planned {
        if release.metadata.revoked {
            continue;
        }
        for artifact in [&release.artifact, &release.metadata.launcher] {
            let (version, basename) = artifact.name.split_once('/').ok_or(Error::Manifest)?;
            let directory = mirror.join(version);
            create_private_directory(&directory)?;
            let raw_name = format!("{}.{basename}", artifact.sha256);
            // The copied bytes are independently checked again through the
            // actual client transport. Links are never used as mirror entries.
            std::fs::copy(config.assets.join(&raw_name), directory.join(raw_name))?;
        }
    }
    let root_path = staging.path.join("root.json");
    std::fs::write(&root_path, &root_bytes)?;
    let metadata = staging.path.join("metadata");
    create_private_directory(&metadata)?;
    let mut editor = RepositoryEditor::new(&root_path)
        .await
        .map_err(|_| Error::Metadata)?;
    let targets_expires = config
        .targets_expires
        .parse()
        .map_err(|_| Error::State("invalid targets expiration"))?;
    editor
        .targets(Signed {
            signed: Targets {
                spec_version: "1.0.0".to_owned(),
                version: sequence,
                expires: targets_expires,
                targets: std::collections::HashMap::new(),
                delegations: None,
                _extra: std::collections::HashMap::new(),
            },
            signatures: Vec::new(),
        })
        .map_err(|_| Error::Metadata)?;
    editor
        .targets_version(sequence)
        .map_err(|_| Error::Metadata)?
        .targets_expires(targets_expires)
        .map_err(|_| Error::Metadata)?
        .snapshot_version(sequence)
        .snapshot_expires(
            config
                .snapshot_expires
                .parse()
                .map_err(|_| Error::State("invalid snapshot expiration"))?,
        )
        .timestamp_version(sequence)
        .timestamp_expires(
            config
                .timestamp_expires
                .parse()
                .map_err(|_| Error::State("invalid timestamp expiration"))?,
        );
    for (name, descriptor) in &input.targets {
        let value = serde_json::to_value(descriptor).map_err(|_| Error::Manifest)?;
        let target: Target = serde_json::from_value(value).map_err(|_| Error::Manifest)?;
        editor
            .add_target(
                tough::TargetName::new(name).map_err(|_| Error::Manifest)?,
                target,
            )
            .map_err(|_| Error::Metadata)?;
    }
    let signed = editor.sign(&keys).await.map_err(|_| Error::Metadata)?;
    signed.write(&metadata).await.map_err(|_| Error::Metadata)?;
    let verified = TrustedRepository::load(RepositoryConfig {
        trusted_root: Some(root_bytes),
        metadata_url: url::Url::from_directory_path(&metadata).map_err(|_| Error::Source)?,
        targets_url: url::Url::from_directory_path(&mirror).map_err(|_| Error::Source)?,
        datastore: staging.path.join("verification-cache"),
        offline: true,
        recover_pending: false,
        max_target_bytes: MAX_TARGET_BYTES,
    })
    .await?;
    if verified.releases() != planned.as_slice() {
        return Err(Error::Integrity);
    }
    for release in &planned {
        if release.metadata.revoked {
            continue;
        }
        for artifact in [&release.artifact, &release.metadata.launcher] {
            let path = staging
                .path
                .join(format!(".verified-{}", uuid::Uuid::now_v7()));
            let mut file = tokio::fs::OpenOptions::new()
                .create_new(true)
                .write(true)
                .open(&path)
                .await?;
            verified.download(artifact, &mut file).await?;
            drop(file);
            std::fs::remove_file(path)?;
        }
    }
    drop(verified);
    // Recheck every raw byte identity after signing, closing accidental release
    // assembly drift while keys were in use. Uploaded assets retain these identities.
    for release in &planned {
        verify_asset(&config.assets, &release.artifact)?;
        verify_asset(&config.assets, &release.metadata.launcher)?;
    }
    for entry in std::fs::read_dir(&metadata)? {
        let entry = entry?;
        ensure_regular_path(&entry.path())?;
        OpenOptions::new()
            .read(true)
            .write(true)
            .open(entry.path())?
            .sync_all()?;
    }
    sync_directory(&metadata)?;
    reject_existing(&config.out)?;
    std::fs::rename(&metadata, &config.out)?;
    sync_directory(parent)
}

fn validate_input(input: &Input, assets: &Path) -> Result<Vec<ReleaseTarget>> {
    ensure_directory(assets)?;
    if input.format_version != 1
        || input.version.major == 0
        || !input.version.build.is_empty()
        || input.version.to_string().len() > 128
        || input.targets.is_empty()
        || input.targets.len() > MAX_TARGETS
    {
        return Err(Error::Manifest);
    }
    let mut releases = Vec::new();
    let mut paired = BTreeSet::new();
    let mut cohorts: BTreeMap<Version, BTreeSet<String>> = BTreeMap::new();
    for (name, descriptor) in &input.targets {
        if descriptor.custom.is_empty() {
            continue;
        }
        if descriptor.custom.len() != 1 {
            return Err(Error::Manifest);
        }
        let value = descriptor.custom.get("knowell").ok_or(Error::Manifest)?;
        let metadata: ReleaseMetadata =
            serde_json::from_value(value.clone()).map_err(|_| Error::Manifest)?;
        metadata.validate(name)?;
        let artifact = Artifact {
            name: name.clone(),
            sha256: descriptor.hashes.sha256.clone(),
            size: descriptor.length,
        };
        artifact.validate(&metadata.version, &metadata.target, Component::Engine)?;
        let launcher = input
            .targets
            .get(&metadata.launcher.name)
            .ok_or(Error::Manifest)?;
        if !launcher.custom.is_empty()
            || launcher.length != metadata.launcher.size
            || launcher.hashes.sha256 != metadata.launcher.sha256
            || !paired.insert(metadata.launcher.name.clone())
        {
            return Err(Error::Integrity);
        }
        verify_asset(assets, &artifact)?;
        verify_asset(assets, &metadata.launcher)?;
        cohorts
            .entry(metadata.version.clone())
            .or_default()
            .insert(metadata.target.clone());
        releases.push(ReleaseTarget { artifact, metadata });
    }
    if releases.is_empty()
        || !releases
            .iter()
            .any(|release| release.metadata.version == input.version)
        || input.targets.len()
            != releases
                .len()
                .checked_add(paired.len())
                .ok_or(Error::Limit)?
    {
        return Err(Error::Manifest);
    }
    if cohorts.values().any(|targets| {
        REQUIRED_TARGETS
            .iter()
            .any(|required| !targets.contains(*required))
    }) {
        return Err(Error::Manifest);
    }
    for release in &releases {
        if release.metadata.channel == Channel::Preview
            && !releases.iter().any(|stable| {
                stable.metadata.target == release.metadata.target
                    && stable.metadata.channel == Channel::Stable
                    && !stable.metadata.revoked
            })
        {
            return Err(Error::PreviewUnavailable);
        }
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

fn verify_asset(assets: &Path, artifact: &Artifact) -> Result<()> {
    let (_, basename) = artifact.name.split_once('/').ok_or(Error::Manifest)?;
    verify_file(
        &assets.join(format!("{}.{basename}", artifact.sha256)),
        artifact.size,
        &artifact.sha256,
    )
}

async fn read_keys(paths: &[PathBuf], root: &Signed<Root>) -> Result<Vec<Box<dyn KeySource>>> {
    if paths.is_empty() || paths.len() > MAX_KEYS {
        return Err(Error::Limit);
    }
    let mut keys: Vec<Box<dyn KeySource>> = Vec::new();
    let mut identities = BTreeSet::new();
    for path in paths {
        let source = MemoryKey(read_bounded(path, MAX_KEY_BYTES)?);
        let signer = source
            .as_sign()
            .await
            .map_err(|_| Error::State("offline signing key is invalid"))?;
        let identity = root.signed.key_id(signer.as_ref()).ok_or(Error::State(
            "offline signing key is not authorized by the root",
        ))?;
        if !identities.insert(identity) {
            return Err(Error::State("duplicate offline signing key"));
        }
        keys.push(Box::new(source));
    }
    for role in [RoleType::Targets, RoleType::Snapshot, RoleType::Timestamp] {
        let authorized = root.signed.roles.get(&role).ok_or(Error::Metadata)?;
        let count = authorized
            .keyids
            .iter()
            .filter(|identity| identities.contains(*identity))
            .count();
        if u64::try_from(count).map_err(|_| Error::Limit)? < authorized.threshold.get() {
            return Err(Error::State(
                "offline signing inventory does not satisfy every role threshold",
            ));
        }
    }
    Ok(keys)
}

fn validate_expirations(config: &PublisherConfig, root: &Signed<Root>) -> Result<()> {
    use time::format_description::well_known::Rfc3339;
    let now = time::OffsetDateTime::now_utc();
    let root_expiration = time::OffsetDateTime::parse(&root.signed.expires.to_string(), &Rfc3339)
        .map_err(|_| Error::Metadata)?;
    let mut previous = root_expiration;
    for (value, days) in [
        (&config.targets_expires, 365),
        (&config.snapshot_expires, 30),
        (&config.timestamp_expires, 7),
    ] {
        if value.len() > 64 {
            return Err(Error::Limit);
        }
        let expiration = time::OffsetDateTime::parse(value, &Rfc3339)
            .map_err(|_| Error::State("role expiration must be an rfc3339 instant"))?;
        if expiration.offset() != time::UtcOffset::UTC
            || expiration <= now
            || expiration > previous
            || expiration - now > time::Duration::days(days)
        {
            return Err(Error::State(
                "role expiration is expired, out of order, or exceeds its maximum lifetime",
            ));
        }
        previous = expiration;
    }
    Ok(())
}

fn validate_public_root(bytes: &[u8]) -> Result<()> {
    // Tough intentionally preserves schema extensions, and its editor retains
    // the original root bytes. An unknown private field must never be copied to
    // a public repository merely because the root's signatures are valid.
    let root: serde_json::Value = serde_json::from_slice(bytes).map_err(|_| Error::Metadata)?;
    public_fields(&root, &["signed", "signatures"])?;
    let signed = root.get("signed").ok_or(Error::Metadata)?;
    public_fields(
        signed,
        &[
            "_type",
            "spec_version",
            "version",
            "expires",
            "consistent_snapshot",
            "keys",
            "roles",
        ],
    )?;
    let keys = signed
        .get("keys")
        .and_then(serde_json::Value::as_object)
        .ok_or(Error::Metadata)?;
    for key in keys.values() {
        public_fields(key, &["keytype", "scheme", "keyval"])?;
        public_fields(key.get("keyval").ok_or(Error::Metadata)?, &["public"])?;
    }
    let roles = signed.get("roles").ok_or(Error::Metadata)?;
    public_fields(roles, &["root", "targets", "snapshot", "timestamp"])?;
    for role in roles.as_object().ok_or(Error::Metadata)?.values() {
        public_fields(role, &["keyids", "threshold"])?;
    }
    for signature in root
        .get("signatures")
        .and_then(serde_json::Value::as_array)
        .ok_or(Error::Metadata)?
    {
        public_fields(signature, &["keyid", "sig"])?;
    }
    Ok(())
}

fn public_fields(value: &serde_json::Value, expected: &[&str]) -> Result<()> {
    let object = value.as_object().ok_or(Error::Metadata)?;
    if object.len() != expected.len() || expected.iter().any(|field| !object.contains_key(*field)) {
        return Err(Error::State(
            "public root contains unsupported fields; review its public-only signing ceremony",
        ));
    }
    Ok(())
}

fn read_bounded(path: &Path, maximum: u64) -> Result<Vec<u8>> {
    ensure_regular_path(path)?;
    let file = File::open(path)?;
    if file.metadata()?.len() > maximum {
        return Err(Error::Limit);
    }
    let mut bytes = Vec::new();
    file.take(maximum + 1).read_to_end(&mut bytes)?;
    if u64::try_from(bytes.len()).map_err(|_| Error::Limit)? > maximum {
        return Err(Error::Limit);
    }
    Ok(bytes)
}

fn reject_existing(path: &Path) -> Result<()> {
    match std::fs::symlink_metadata(path) {
        Ok(_) => Err(Error::State("publisher output must be a fresh directory")),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(Error::Io(error)),
    }
}

struct OutputStage {
    path: PathBuf,
    parent: PathBuf,
}

impl OutputStage {
    fn new(parent: &Path) -> Result<Self> {
        let parent = parent.canonicalize()?;
        let path = parent.join(format!(".knowell-publisher-{}", uuid::Uuid::now_v7()));
        create_private_directory(&path)?;
        Ok(Self { path, parent })
    }
}

impl Drop for OutputStage {
    fn drop(&mut self) {
        // Only this create-new owned staging subtree can be removed, never the
        // user-selected output or its parent. No signing key is copied here.
        if self.path.parent() == Some(self.parent.as_path())
            && self
                .path
                .file_name()
                .is_some_and(|name| name.to_string_lossy().starts_with(".knowell-publisher-"))
            && ensure_directory(&self.path).is_ok()
            && self
                .path
                .canonicalize()
                .is_ok_and(|path| path == self.path && path.starts_with(&self.parent))
        {
            let _ = std::fs::remove_dir_all(&self.path);
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use sha2::{Digest, Sha256};
    use std::collections::HashMap;
    use tough::editor::signed::SignedRole;
    use tough::schema::{KeyHolder, RoleKeys};

    struct Fixture {
        _temporary: tempfile::TempDir,
        config: PublisherConfig,
    }

    impl Fixture {
        async fn new() -> Self {
            let temporary = tempfile::tempdir().unwrap();
            let base = temporary.path().canonicalize().unwrap();
            let mut key_paths = Vec::new();
            let mut sources: Vec<Box<dyn KeySource>> = Vec::new();
            let mut public = HashMap::new();
            let mut ids = Vec::new();
            for index in 0..2 {
                let path = base.join(format!("synthetic-offline-key-{index}.der"));
                let key = aws_lc_rs::signature::Ed25519KeyPair::generate_pkcs8(
                    &aws_lc_rs::rand::SystemRandom::new(),
                )
                .unwrap();
                std::fs::write(&path, key.as_ref()).unwrap();
                let source = MemoryKey(key.as_ref().to_vec());
                let public_key = source.as_sign().await.unwrap().tuf_key();
                let id = public_key.key_id().unwrap();
                ids.push(id.clone());
                public.insert(id, public_key);
                sources.push(Box::new(source));
                key_paths.push(path);
            }
            let role = RoleKeys {
                keyids: ids,
                threshold: NonZeroU64::new(2).unwrap(),
                _extra: HashMap::new(),
            };
            let root = Root {
                spec_version: "1.0.0".to_owned(),
                consistent_snapshot: true,
                version: NonZeroU64::new(1).unwrap(),
                expires: "2100-01-01T00:00:00Z".parse().unwrap(),
                keys: public,
                roles: [
                    RoleType::Root,
                    RoleType::Targets,
                    RoleType::Snapshot,
                    RoleType::Timestamp,
                ]
                .into_iter()
                .map(|kind| (kind, role.clone()))
                .collect(),
                _extra: HashMap::new(),
            };
            let root = SignedRole::new(
                root.clone(),
                &KeyHolder::Root(root),
                &sources,
                &aws_lc_rs::rand::SystemRandom::new(),
            )
            .await
            .unwrap();
            let root_path = base.join("public-root.json");
            std::fs::write(&root_path, root.buffer()).unwrap();
            let assets = base.join("assets");
            std::fs::create_dir(&assets).unwrap();
            let version = Version::parse("1.0.0").unwrap();
            let mut descriptors = serde_json::Map::new();
            for target in REQUIRED_TARGETS {
                let mut artifacts = Vec::new();
                for (component, bytes) in [
                    (Component::Engine, b"synthetic engine".as_slice()),
                    (Component::Launcher, b"synthetic launcher".as_slice()),
                ] {
                    let artifact = Artifact {
                        name: crate::manifest::target_name(&version, target, component).unwrap(),
                        sha256: Sha256::digest(bytes)
                            .iter()
                            .map(|byte| format!("{byte:02x}"))
                            .collect(),
                        size: u64::try_from(bytes.len()).unwrap(),
                    };
                    let (_, basename) = artifact.name.split_once('/').unwrap();
                    std::fs::write(
                        assets.join(format!("{}.{basename}", artifact.sha256)),
                        bytes,
                    )
                    .unwrap();
                    descriptors.insert(artifact.name.clone(), serde_json::json!({"length": artifact.size,"hashes":{"sha256":artifact.sha256},"custom":{}}));
                    artifacts.push(artifact);
                }
                let engine = artifacts.first().unwrap();
                let launcher = artifacts.get(1).unwrap();
                descriptors.get_mut(&engine.name).unwrap().as_object_mut().unwrap().insert("custom".to_owned(), serde_json::json!({"knowell":{
                "format_version":1,"version":"1.0.0","target":target,"channel":"stable","component":"engine","revoked":false,"launcher":launcher,
                "compatibility":{"schema":{"read_min":1,"read_max":1,"write_min":1,"write_max":1},
                    "config":{"min":1,"max":1},"index":{"min":1,"max":1},"jobs":{"min":1,"max":1},"protocol":{"min":1,"max":1},"launcher":{"min":1,"max":1}}
            }}));
            }
            let targets = base.join("unsigned.json");
            std::fs::write(&targets, serde_json::to_vec(&serde_json::json!({"format_version":1,"version":"1.0.0","targets":descriptors})).unwrap()).unwrap();
            let expiration = |days| {
                (time::OffsetDateTime::now_utc() + time::Duration::days(days))
                    .format(&time::format_description::well_known::Rfc3339)
                    .unwrap()
            };
            Self {
                _temporary: temporary,
                config: PublisherConfig {
                    root: root_path,
                    targets,
                    keys: key_paths,
                    sequence: 1,
                    targets_expires: expiration(5),
                    snapshot_expires: expiration(2),
                    timestamp_expires: expiration(1),
                    assets,
                    out: base.join("published"),
                },
            }
        }
    }

    #[tokio::test]
    async fn two_key_threshold_publication_verifies_without_publishing_cache_or_keys() {
        let fixture = Fixture::new().await;
        publish(fixture.config.clone()).await.unwrap();
        let names: BTreeSet<_> = std::fs::read_dir(&fixture.config.out)
            .unwrap()
            .map(|entry| entry.unwrap().file_name().into_string().unwrap())
            .collect();
        assert_eq!(
            names,
            BTreeSet::from([
                "1.root.json".to_owned(),
                "1.targets.json".to_owned(),
                "1.snapshot.json".to_owned(),
                "timestamp.json".to_owned()
            ])
        );
        for role in ["1.targets.json", "1.snapshot.json", "timestamp.json"] {
            let value: serde_json::Value =
                serde_json::from_slice(&std::fs::read(fixture.config.out.join(role)).unwrap())
                    .unwrap();
            assert_eq!(
                value.get("signatures").unwrap().as_array().unwrap().len(),
                2
            );
        }
        assert!(matches!(
            publish(fixture.config.clone()).await,
            Err(Error::State(_))
        ));
    }

    #[tokio::test]
    async fn missing_threshold_wrong_or_duplicate_keys_never_publish() {
        let fixture = Fixture::new().await;
        let mut missing = fixture.config.clone();
        missing.keys.truncate(1);
        assert!(matches!(publish(missing).await, Err(Error::State(_))));
        assert!(!fixture.config.out.exists());
        let mut duplicate = fixture.config.clone();
        duplicate.keys = vec![duplicate.keys.first().unwrap().clone(); 2];
        assert!(matches!(publish(duplicate).await, Err(Error::State(_))));
        let wrong = fixture.config.clone();
        let key = aws_lc_rs::signature::Ed25519KeyPair::generate_pkcs8(
            &aws_lc_rs::rand::SystemRandom::new(),
        )
        .unwrap();
        std::fs::write(wrong.keys.first().unwrap(), key.as_ref()).unwrap();
        assert!(matches!(publish(wrong.clone()).await, Err(Error::State(_))));
        assert!(!wrong.out.exists());
        assert!(!format!("{:?}", wrong).contains("synthetic-offline-key"));
    }

    #[tokio::test]
    async fn hostile_input_and_changed_raw_bytes_fail_before_publication() {
        let fixture = Fixture::new().await;
        for bytes in [b"{".as_slice(), br#"{"format_version":1,"format_version":1}"#, br#"{"format_version":1,"version":"1.0.0","targets":{"../../escape":{"length":1,"hashes":{"sha256":"bad"},"custom":{}}}}"#] {
            std::fs::write(&fixture.config.targets, bytes).unwrap();
            assert!(publish(fixture.config.clone()).await.is_err());
            assert!(!fixture.config.out.exists());
        }
        let fixture = Fixture::new().await;
        let raw = std::fs::read_dir(&fixture.config.assets)
            .unwrap()
            .next()
            .unwrap()
            .unwrap()
            .path();
        std::fs::write(raw, b"changed").unwrap();
        assert!(matches!(
            publish(fixture.config.clone()).await,
            Err(Error::Integrity)
        ));
        assert!(!fixture.config.out.exists());
        let mut expired = fixture.config.clone();
        expired.timestamp_expires = "2000-01-01T00:00:00Z".to_owned();
        assert!(matches!(publish(expired).await, Err(Error::State(_))));
    }

    #[tokio::test]
    async fn incomplete_platform_cohort_and_unapproved_preview_never_publish() {
        let fixture = Fixture::new().await;
        let original = std::fs::read(&fixture.config.targets).unwrap();
        let mut input: serde_json::Value = serde_json::from_slice(&original).unwrap();
        let targets = input.get_mut("targets").unwrap().as_object_mut().unwrap();
        targets.retain(|name, _| !name.contains("aarch64-apple-darwin"));
        std::fs::write(&fixture.config.targets, serde_json::to_vec(&input).unwrap()).unwrap();
        assert!(matches!(
            publish(fixture.config.clone()).await,
            Err(Error::Manifest)
        ));
        assert!(!fixture.config.out.exists());
        // Preview naming must be canonical too, so rebuild both descriptor paths
        // and signed custom identities before testing the stable-first gate.
        let text = String::from_utf8(original)
            .unwrap()
            .replace("1.0.0", "1.1.0-rc.1")
            .replace("\"stable\"", "\"preview\"");
        for entry in std::fs::read_dir(&fixture.config.assets).unwrap() {
            let entry = entry.unwrap();
            let name = entry
                .file_name()
                .into_string()
                .unwrap()
                .replace("1.0.0", "1.1.0-rc.1");
            std::fs::copy(entry.path(), fixture.config.assets.join(name)).unwrap();
        }
        std::fs::write(&fixture.config.targets, text).unwrap();
        assert!(matches!(
            publish(fixture.config.clone()).await,
            Err(Error::PreviewUnavailable)
        ));
        assert!(!fixture.config.out.exists());
    }

    #[tokio::test]
    async fn cumulative_revoked_cohorts_are_signed_but_never_executed_or_downloaded() {
        let fixture = Fixture::new().await;
        let mut input: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&fixture.config.targets).unwrap()).unwrap();
        let newer = input.get("targets").unwrap().as_object().unwrap().clone();
        let targets = input.get_mut("targets").unwrap().as_object_mut().unwrap();
        for descriptor in targets.values_mut() {
            if let Some(custom) = descriptor.get_mut("custom").unwrap().get_mut("knowell") {
                custom
                    .as_object_mut()
                    .unwrap()
                    .insert("revoked".to_owned(), true.into());
            }
        }
        for (name, descriptor) in newer {
            let next = serde_json::to_string(&descriptor)
                .unwrap()
                .replace("1.0.0", "1.1.0");
            targets.insert(
                name.replace("1.0.0", "1.1.0"),
                serde_json::from_str(&next).unwrap(),
            );
        }
        input
            .as_object_mut()
            .unwrap()
            .insert("version".to_owned(), "1.1.0".into());
        let raw: Vec<_> = std::fs::read_dir(&fixture.config.assets)
            .unwrap()
            .map(|entry| entry.unwrap())
            .collect();
        for entry in raw {
            let name = entry
                .file_name()
                .into_string()
                .unwrap()
                .replace("1.0.0", "1.1.0");
            std::fs::copy(entry.path(), fixture.config.assets.join(name)).unwrap();
        }
        std::fs::write(&fixture.config.targets, serde_json::to_vec(&input).unwrap()).unwrap();
        publish(fixture.config.clone()).await.unwrap();
        let signed: Signed<Targets> = serde_json::from_slice(
            &std::fs::read(fixture.config.out.join("1.targets.json")).unwrap(),
        )
        .unwrap();
        assert_eq!(signed.signed.targets.len(), 24);
        let revoked = signed
            .signed
            .targets
            .values()
            .filter(|target| {
                target
                    .custom
                    .get("knowell")
                    .is_some_and(|custom| custom.get("revoked") == Some(&true.into()))
            })
            .count();
        assert_eq!(revoked, 6);
    }

    #[tokio::test]
    async fn valid_root_signatures_cannot_publish_private_or_unknown_fields() {
        let fixture = Fixture::new().await;
        let original = std::fs::read(&fixture.config.root).unwrap();
        let canary = "KNOWELL_CANARY_FAKE_PRIVATE_MUST_NEVER_BE_PUBLISHED";
        let mut envelope: serde_json::Value = serde_json::from_slice(&original).unwrap();
        envelope
            .as_object_mut()
            .unwrap()
            .insert("private".to_owned(), canary.into());
        let bytes = serde_json::to_vec(&envelope).unwrap();
        validate_root(&bytes).unwrap();
        std::fs::write(&fixture.config.root, bytes).unwrap();
        let error = publish(fixture.config.clone()).await.unwrap_err();
        assert!(matches!(error, Error::State(_)));
        assert!(!format!("{error:?} {error}").contains(canary));
        assert!(!fixture.config.out.exists());
        let previous: Signed<Root> = serde_json::from_slice(&original).unwrap();
        let mut modified = previous.signed.clone();
        // Preserve the root quorum's canonical key identities. The additional
        // targets-authorized key has its own correct ID including the extension.
        let mut extra_key = modified.keys.values().next().unwrap().clone();
        match &mut extra_key {
            tough::schema::key::Key::Rsa { keyval, .. } => {
                keyval._extra.insert("private".to_owned(), canary.into());
            }
            tough::schema::key::Key::Ed25519 { keyval, .. } => {
                keyval._extra.insert("private".to_owned(), canary.into());
            }
            tough::schema::key::Key::Ecdsa { keyval, .. }
            | tough::schema::key::Key::EcdsaOld { keyval, .. } => {
                keyval._extra.insert("private".to_owned(), canary.into());
            }
        }
        let extra_id = extra_key.key_id().unwrap();
        modified.keys.insert(extra_id.clone(), extra_key);
        modified
            .roles
            .get_mut(&RoleType::Targets)
            .unwrap()
            .keyids
            .push(extra_id);
        let sources: Vec<Box<dyn KeySource>> = fixture
            .config
            .keys
            .iter()
            .map(|path| Box::new(MemoryKey(std::fs::read(path).unwrap())) as Box<dyn KeySource>)
            .collect();
        let signed = SignedRole::new(
            modified,
            &KeyHolder::Root(previous.signed),
            &sources,
            &aws_lc_rs::rand::SystemRandom::new(),
        )
        .await
        .unwrap();
        validate_root(signed.buffer()).unwrap();
        std::fs::write(&fixture.config.root, signed.buffer()).unwrap();
        let error = publish(fixture.config.clone()).await.unwrap_err();
        assert!(matches!(error, Error::State(_)));
        assert!(!format!("{error:?} {error}").contains(canary));
        assert!(!fixture.config.out.exists());
    }
}
