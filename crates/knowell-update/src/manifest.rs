//! Exact release identities and explicit format compatibility.

use semver::Version;
use serde::{Deserialize, Serialize};

use crate::{Error, Result};

/// The release channel. Stable never selects a prerelease.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Channel {
    /// Public stable releases, starting with 1.0.0.
    Stable,
    /// Explicitly opted-in preview releases, after stable 1.0 exists.
    Preview,
}

/// The executable described by an artifact identity.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Component {
    /// The full Knowell engine.
    Engine,
    /// The stable admission and execution launcher.
    Launcher,
}

/// An immutable raw TUF target; size is bytes and sha256 is lowercase hexadecimal.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Artifact {
    /// Exact controlled relative TUF target name, including the release directory.
    pub name: String,
    /// SHA-256 encoded as exactly 64 lowercase hexadecimal characters.
    pub sha256: String,
    /// Exact executable size in bytes.
    pub size: u64,
}

impl Artifact {
    /// Checks bounded size, digest encoding, and canonical component identity.
    pub fn validate(&self, version: &Version, target: &str, component: Component) -> Result<()> {
        if self.name != target_name(version, target, component)?
            || !valid_digest(&self.sha256)
            || self.size == 0
            || self.size > MAX_TARGET_BYTES
        {
            return Err(Error::Manifest);
        }
        Ok(())
    }
}

/// Absolute executable transfer ceiling, in bytes (1 GiB).
pub const MAX_TARGET_BYTES: u64 = 1024 * 1024 * 1024;

/// Inclusive supported format range; versions are nonnegative format identifiers.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct VersionRange {
    /// Oldest accepted format, inclusive.
    pub min: u32,
    /// Newest accepted format, inclusive.
    pub max: u32,
}

impl VersionRange {
    /// Returns whether a format identifier lies inside the inclusive range.
    pub fn contains(self, version: u32) -> bool {
        self.min <= version && version <= self.max
    }

    fn valid(self) -> bool {
        self.min <= self.max
    }
}

/// Inclusive database schema ranges for reading and writing.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SchemaRange {
    /// Oldest schema that can be read.
    pub read_min: u32,
    /// Newest schema that can be read.
    pub read_max: u32,
    /// Oldest schema that can be written.
    pub write_min: u32,
    /// Newest schema that can be written.
    pub write_max: u32,
}

impl SchemaRange {
    fn valid(self) -> bool {
        self.read_min <= self.write_min
            && self.write_min <= self.write_max
            && self.write_max <= self.read_max
    }

    fn accepts(self, version: u32) -> bool {
        self.read_min <= version
            && version <= self.read_max
            && self.write_min <= version
            && version <= self.write_max
    }
}

/// Signed compatibility contract; none of these fields grants migration permission.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Compatibility {
    /// Database schema identifiers accepted for reads and writes.
    pub schema: SchemaRange,
    /// Persisted configuration format identifiers.
    pub config: VersionRange,
    /// Persisted lexical/index format identifiers.
    pub index: VersionRange,
    /// Durable job payload format identifiers.
    pub jobs: VersionRange,
    /// Hub/worker/edge wire protocol identifiers.
    pub protocol: VersionRange,
    /// Launcher admission protocol identifiers.
    pub launcher: VersionRange,
}

/// Observed persisted formats, captured again under exclusive admission before apply.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RuntimeState {
    /// Current database schema identifier.
    pub schema: u32,
    /// Current configuration format identifier.
    pub config: u32,
    /// Current lexical/index format identifier.
    pub index: u32,
    /// Current durable job payload identifier.
    pub jobs: u32,
    /// Current deployment wire protocol identifier.
    pub protocol: u32,
    /// Installed launcher admission protocol identifier.
    pub launcher: u32,
}

impl Compatibility {
    /// Rejects invalid ranges in signed metadata.
    pub fn validate(&self) -> Result<()> {
        if !self.schema.valid()
            || !self.config.valid()
            || !self.index.valid()
            || !self.jobs.valid()
            || !self.protocol.valid()
            || !self.launcher.valid()
        {
            return Err(Error::Manifest);
        }
        Ok(())
    }

    /// Checks compatibility without changing any data or authorizing migrations.
    pub fn check(&self, state: RuntimeState) -> Result<()> {
        self.validate()?;
        if !self.schema.accepts(state.schema) {
            return Err(Error::Compatibility(
                "database schema requires explicit maintenance",
            ));
        }
        for (range, value, reason) in [
            (
                self.config,
                state.config,
                "configuration format is unsupported",
            ),
            (self.index, state.index, "index format is unsupported"),
            (self.jobs, state.jobs, "durable job format is unsupported"),
            (
                self.protocol,
                state.protocol,
                "deployment protocol is unsupported",
            ),
            (
                self.launcher,
                state.launcher,
                "launcher protocol is unsupported",
            ),
        ] {
            if !range.contains(value) {
                return Err(Error::Compatibility(reason));
            }
        }
        Ok(())
    }

    /// A binary rollback is eligible only when every current persisted format is accepted.
    ///
    /// This does not authorize restoring a database backup or discarding later writes.
    pub fn rollback_allowed(&self, state: RuntimeState) -> bool {
        self.check(state).is_ok()
    }
}

/// Strict custom `knowell` metadata carried by a signed engine TUF target.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReleaseMetadata {
    /// Manifest format identifier; currently exactly 1.
    pub format_version: u32,
    /// Exact semantic release version, without build metadata.
    pub version: Version,
    /// Exact Rust platform target triple.
    pub target: String,
    /// Authorized release channel.
    pub channel: Channel,
    /// Must be `engine`; the paired launcher is bound separately.
    pub component: Component,
    /// Explicit supported persisted-format ranges.
    pub compatibility: Compatibility,
    /// Exact paired launcher target; must also exist in signed TUF targets.
    pub launcher: Artifact,
    /// Whether this engine release has been withdrawn.
    pub revoked: bool,
}

impl ReleaseMetadata {
    /// Checks all signed fields and the exact engine target name.
    pub fn validate(&self, name: &str) -> Result<()> {
        if self.format_version != 1
            || self.component != Component::Engine
            || self.version.major == 0
            || self.version.to_string().len() > 128
            || !self.version.build.is_empty()
            || (self.channel == Channel::Stable && !self.version.pre.is_empty())
            || (self.channel == Channel::Preview && self.version.pre.is_empty())
            || name != target_name(&self.version, &self.target, Component::Engine)?
        {
            return Err(Error::Manifest);
        }
        self.compatibility.validate()?;
        self.launcher
            .validate(&self.version, &self.target, Component::Launcher)
    }
}

/// A verified signed engine descriptor and its paired launcher identity.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReleaseTarget {
    /// Exact engine target identity from TUF's signed length and SHA-256.
    pub artifact: Artifact,
    /// Strict Knowell metadata from that same signed target.
    pub metadata: ReleaseMetadata,
}

/// Selection never falls back from an explicitly pinned version.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Selection {
    /// Highest eligible semantic version for exactly the requested channel and target.
    Latest,
    /// Exactly this semantic version; metadata must name it without substitution.
    Pinned(Version),
}

/// Selects a signed engine target deterministically, with explicit downgrade policy.
pub fn select_release(
    releases: &[ReleaseTarget],
    current: &Version,
    target: &str,
    channel: Channel,
    selection: &Selection,
    allow_downgrade: bool,
) -> Result<ReleaseTarget> {
    if !valid_target(target) {
        return Err(Error::Manifest);
    }
    if current.major == 0 && matches!(selection, Selection::Latest) {
        return Err(Error::Development);
    }
    if allow_downgrade && matches!(selection, Selection::Latest) {
        return Err(Error::Downgrade);
    }
    for release in releases {
        release.metadata.validate(&release.artifact.name)?;
        release.artifact.validate(
            &release.metadata.version,
            &release.metadata.target,
            Component::Engine,
        )?;
    }
    if channel == Channel::Preview
        && !releases.iter().any(|release| {
            release.metadata.target == target
                && release.metadata.channel == Channel::Stable
                && !release.metadata.revoked
        })
    {
        return Err(Error::PreviewUnavailable);
    }
    let mut eligible: Vec<_> = releases
        .iter()
        .filter(|release| release.metadata.target == target && release.metadata.channel == channel)
        .filter(|release| match selection {
            Selection::Latest => !release.metadata.revoked,
            Selection::Pinned(version) => release.metadata.version == *version,
        })
        .collect();
    eligible.sort_by(|left, right| {
        left.metadata
            .version
            .cmp(&right.metadata.version)
            .then_with(|| left.artifact.name.cmp(&right.artifact.name))
    });
    if eligible.windows(2).any(|pair| {
        pair.first()
            .zip(pair.get(1))
            .is_some_and(|(left, right)| left.metadata.version == right.metadata.version)
    }) {
        return Err(Error::Manifest);
    }
    let selected = eligible.last().ok_or(Error::ReleaseUnavailable)?;
    if selected.metadata.revoked {
        return Err(Error::Revoked);
    }
    if selected.metadata.version < *current
        && (!allow_downgrade || matches!(selection, Selection::Latest))
    {
        return Err(Error::Downgrade);
    }
    Ok((*selected).clone())
}

/// Constructs the only permitted raw executable target path.
pub fn target_name(version: &Version, target: &str, component: Component) -> Result<String> {
    if !valid_target(target) || !version.build.is_empty() || version.to_string().len() > 128 {
        return Err(Error::Manifest);
    }
    let component = match component {
        Component::Engine => "engine",
        Component::Launcher => "launcher",
    };
    let suffix = if target.ends_with("-windows-msvc") {
        ".exe"
    } else {
        ""
    };
    Ok(format!(
        "v{version}/knowell-{version}-{target}-{component}{suffix}"
    ))
}

pub(crate) fn valid_digest(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

fn valid_target(target: &str) -> bool {
    matches!(
        target,
        "x86_64-unknown-linux-gnu"
            | "aarch64-unknown-linux-gnu"
            | "x86_64-unknown-linux-musl"
            | "aarch64-unknown-linux-musl"
            | "x86_64-apple-darwin"
            | "aarch64-apple-darwin"
            | "x86_64-pc-windows-msvc"
            | "aarch64-pc-windows-msvc"
    )
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    fn release(version: &str, channel: Channel) -> ReleaseTarget {
        let version = Version::parse(version).unwrap();
        let target = "x86_64-unknown-linux-gnu";
        let range = VersionRange { min: 1, max: 1 };
        ReleaseTarget {
            artifact: Artifact {
                name: target_name(&version, target, Component::Engine).unwrap(),
                sha256: "a".repeat(64),
                size: 3,
            },
            metadata: ReleaseMetadata {
                format_version: 1,
                launcher: Artifact {
                    name: target_name(&version, target, Component::Launcher).unwrap(),
                    sha256: "b".repeat(64),
                    size: 2,
                },
                version,
                target: target.to_owned(),
                channel,
                component: Component::Engine,
                revoked: false,
                compatibility: Compatibility {
                    schema: SchemaRange {
                        read_min: 1,
                        read_max: 2,
                        write_min: 1,
                        write_max: 1,
                    },
                    config: range,
                    index: range,
                    jobs: range,
                    protocol: range,
                    launcher: range,
                },
            },
        }
    }

    #[test]
    fn exact_selection_never_substitutes_version_channel_or_target() {
        let releases = vec![
            release("1.2.0", Channel::Stable),
            release("1.10.0", Channel::Stable),
        ];
        let current = Version::parse("1.0.0").unwrap();
        let selected = select_release(
            &releases,
            &current,
            "x86_64-unknown-linux-gnu",
            Channel::Stable,
            &Selection::Latest,
            false,
        )
        .unwrap();
        assert_eq!(selected.metadata.version, Version::parse("1.10.0").unwrap());
        assert!(matches!(
            select_release(
                &releases,
                &current,
                "aarch64-apple-darwin",
                Channel::Stable,
                &Selection::Latest,
                false
            ),
            Err(Error::ReleaseUnavailable)
        ));
        assert!(matches!(
            select_release(
                &releases,
                &current,
                "x86_64-unknown-linux-gnu",
                Channel::Stable,
                &Selection::Pinned(Version::parse("1.1.0").unwrap()),
                false
            ),
            Err(Error::ReleaseUnavailable)
        ));
    }

    #[test]
    fn prerelease_policy_and_explicit_downgrade_are_enforced() {
        let preview = release("1.1.0-rc.1", Channel::Preview);
        let current = Version::parse("1.2.0").unwrap();
        assert!(matches!(
            select_release(
                std::slice::from_ref(&preview),
                &current,
                &preview.metadata.target,
                Channel::Preview,
                &Selection::Latest,
                false
            ),
            Err(Error::PreviewUnavailable)
        ));
        let releases = vec![release("1.0.0", Channel::Stable), preview];
        assert!(matches!(
            select_release(
                &releases,
                &current,
                "x86_64-unknown-linux-gnu",
                Channel::Stable,
                &Selection::Latest,
                true
            ),
            Err(Error::Downgrade)
        ));
        assert!(
            select_release(
                &releases,
                &current,
                "x86_64-unknown-linux-gnu",
                Channel::Stable,
                &Selection::Pinned(Version::parse("1.0.0").unwrap()),
                true
            )
            .is_ok()
        );
        let development = Version::parse("0.0.0").unwrap();
        assert!(matches!(
            select_release(
                &releases,
                &development,
                "x86_64-unknown-linux-gnu",
                Channel::Stable,
                &Selection::Latest,
                false
            ),
            Err(Error::Development)
        ));
    }

    #[test]
    fn revoked_release_and_hostile_manifest_fail_closed() {
        let mut candidate = release("1.0.0", Channel::Stable);
        candidate.metadata.revoked = true;
        let current = Version::parse("1.0.0").unwrap();
        assert!(matches!(
            select_release(
                std::slice::from_ref(&candidate),
                &current,
                &candidate.metadata.target,
                Channel::Stable,
                &Selection::Pinned(current.clone()),
                false
            ),
            Err(Error::Revoked)
        ));
        candidate.artifact.name = "../../know".to_owned();
        assert!(matches!(
            candidate.metadata.validate(&candidate.artifact.name),
            Err(Error::Manifest)
        ));
        assert!(serde_json::from_str::<ReleaseMetadata>("{\"format_version\":1").is_err());
        let mut json = serde_json::to_value(release("1.0.0", Channel::Stable).metadata).unwrap();
        json.as_object_mut()
            .unwrap()
            .insert("unknown".to_owned(), true.into());
        assert!(serde_json::from_value::<ReleaseMetadata>(json).is_err());
    }

    #[test]
    fn rollback_requires_write_compatibility_after_migration() {
        let candidate = release("1.0.0", Channel::Stable);
        let state = RuntimeState {
            schema: 1,
            config: 1,
            index: 1,
            jobs: 1,
            protocol: 1,
            launcher: 1,
        };
        assert!(candidate.metadata.compatibility.rollback_allowed(state));
        assert!(
            !candidate
                .metadata
                .compatibility
                .rollback_allowed(RuntimeState { schema: 2, ..state })
        );
        assert!(
            !candidate
                .metadata
                .compatibility
                .rollback_allowed(RuntimeState { jobs: 2, ..state })
        );
    }

    #[test]
    fn hostile_semantic_versions_and_artifact_limits_are_rejected() {
        let mut candidate = release("1.0.0", Channel::Stable);
        let oversized = Version::parse(&format!("1.0.0-{}", "a".repeat(128))).unwrap();
        assert!(matches!(
            target_name(&oversized, &candidate.metadata.target, Component::Engine),
            Err(Error::Manifest)
        ));
        candidate.metadata.version = oversized;
        candidate.metadata.channel = Channel::Preview;
        assert!(matches!(
            candidate.metadata.validate(&candidate.artifact.name),
            Err(Error::Manifest)
        ));
        let mut candidate = release("1.0.0", Channel::Stable);
        candidate.artifact.size = MAX_TARGET_BYTES + 1;
        assert!(matches!(
            candidate.artifact.validate(
                &candidate.metadata.version,
                &candidate.metadata.target,
                Component::Engine
            ),
            Err(Error::Manifest)
        ));
        candidate.artifact.size = 0;
        assert!(matches!(
            candidate.artifact.validate(
                &candidate.metadata.version,
                &candidate.metadata.target,
                Component::Engine
            ),
            Err(Error::Manifest)
        ));
        candidate.artifact.size = 1;
        candidate.artifact.sha256 = "A".repeat(64);
        assert!(matches!(
            candidate.artifact.validate(
                &candidate.metadata.version,
                &candidate.metadata.target,
                Component::Engine
            ),
            Err(Error::Manifest)
        ));
    }
}
