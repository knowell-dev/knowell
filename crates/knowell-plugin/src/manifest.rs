//! Plugin manifests: who a plugin is, which exact component bytes it is, which
//! API version it targets and which capabilities it asks for.

use std::collections::BTreeSet;
use std::fmt;
use std::str::FromStr;

use knowell_core::Name;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::error::PluginError;
use crate::sanitize::sanitize;

/// The plugin API version this host implements: the version of the
/// `knowell:plugin` WIT package in `wit/plugin.wit`.
pub const API_VERSION: ApiVersion = ApiVersion::new(0, 1, 0);

/// The WIT contract plugins are built against (`wit/plugin.wit`), for tooling
/// that hands it to plugin authors.
pub const WIT: &str = include_str!("../wit/plugin.wit");

/// A `MAJOR.MINOR.PATCH` plugin API version.
///
/// Compatibility follows Cargo's caret rule against the host version: same
/// major version, the same minor version while the major version is 0, and
/// not newer than the host (a plugin cannot use functions the host lacks).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct ApiVersion {
    major: u32,
    minor: u32,
    patch: u32,
}

impl ApiVersion {
    /// Creates a version.
    pub const fn new(major: u32, minor: u32, patch: u32) -> Self {
        Self {
            major,
            minor,
            patch,
        }
    }

    /// Major component.
    pub fn major(self) -> u32 {
        self.major
    }

    /// Minor component.
    pub fn minor(self) -> u32 {
        self.minor
    }

    /// Patch component.
    pub fn patch(self) -> u32 {
        self.patch
    }

    /// Whether a plugin built against `self` can run on a host implementing `host`.
    pub fn is_compatible_with(self, host: ApiVersion) -> bool {
        self.major == host.major && (host.major != 0 || self.minor == host.minor) && self <= host
    }
}

impl fmt::Display for ApiVersion {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}.{}.{}", self.major, self.minor, self.patch)
    }
}

impl FromStr for ApiVersion {
    type Err = PluginError;

    /// Parses exactly three dot-separated decimal numbers without leading
    /// zeros (no pre-release or build suffixes).
    fn from_str(text: &str) -> Result<Self, Self::Err> {
        let invalid = || {
            PluginError::InvalidManifest(format!(
                "api-version `{}` must be MAJOR.MINOR.PATCH",
                sanitize(text, 32)
            ))
        };
        let mut parts = text.split('.');
        let mut next = || -> Result<u32, PluginError> {
            let part = parts.next().ok_or_else(invalid)?;
            if part.is_empty()
                || !part.bytes().all(|b| b.is_ascii_digit())
                || (part.len() > 1 && part.starts_with('0'))
            {
                return Err(invalid());
            }
            part.parse().map_err(|_| invalid())
        };
        let version = Self::new(next()?, next()?, next()?);
        if parts.next().is_some() {
            return Err(invalid());
        }
        Ok(version)
    }
}

/// A SHA-256 digest, written as 64 hexadecimal digits (lowercase when
/// displayed; uppercase is accepted when parsing, as some tools print it).
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Sha256Digest([u8; 32]);

impl Sha256Digest {
    /// Hashes `bytes`.
    pub fn of(bytes: &[u8]) -> Self {
        Self(Sha256::digest(bytes).into())
    }

    /// Raw digest bytes.
    pub fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }
}

impl fmt::Display for Sha256Digest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for byte in &self.0 {
            write!(f, "{byte:02x}")?;
        }
        Ok(())
    }
}

impl fmt::Debug for Sha256Digest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Sha256Digest({self})")
    }
}

impl FromStr for Sha256Digest {
    type Err = PluginError;

    fn from_str(text: &str) -> Result<Self, Self::Err> {
        let invalid =
            || PluginError::InvalidManifest("sha256 must be 64 hexadecimal digits".to_string());
        let bytes = text.as_bytes();
        if bytes.len() != 64 {
            return Err(invalid());
        }
        let mut out = [0u8; 32];
        let (pairs, _) = bytes.as_chunks::<2>();
        for (slot, [hi, lo]) in out.iter_mut().zip(pairs) {
            let hi = hex_value(*hi).ok_or_else(invalid)?;
            let lo = hex_value(*lo).ok_or_else(invalid)?;
            *slot = (hi << 4) | lo;
        }
        Ok(Self(out))
    }
}

fn hex_value(c: u8) -> Option<u8> {
    match c {
        b'0'..=b'9' => Some(c - b'0'),
        b'a'..=b'f' => Some(c - b'a' + 10),
        b'A'..=b'F' => Some(c - b'A' + 10),
        _ => None,
    }
}

/// A host capability a plugin can request. Everything not listed here (file
/// system, network, environment variables, real clocks) is never available.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
#[non_exhaustive]
pub enum Capability {
    /// Read-only access to UTF-8 files under the analysed project's root,
    /// through the `knowell:plugin/project-files` interface.
    ProjectFiles,
}

impl Capability {
    /// The manifest / WIT spelling.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::ProjectFiles => "project-files",
        }
    }
}

impl fmt::Display for Capability {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// A validated plugin manifest.
///
/// TOML form (all keys except `capabilities` required, unknown keys rejected):
///
/// ```toml
/// name = "toy-endpoints"         # lowercase slug, 1-64 chars
/// version = "0.1.0"              # the plugin's own semver version
/// api-version = "0.1.0"          # knowell:plugin WIT version it was built against
/// sha256 = "<64 hex digits>"     # digest of the .wasm component
/// capabilities = []              # e.g. ["project-files"]
/// ```
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Manifest {
    name: Name,
    version: String,
    api_version: ApiVersion,
    sha256: Sha256Digest,
    capabilities: BTreeSet<Capability>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields, rename_all = "kebab-case")]
struct RawManifest {
    name: String,
    version: String,
    api_version: String,
    sha256: String,
    #[serde(default)]
    capabilities: Vec<Capability>,
}

impl Manifest {
    /// Largest manifest text accepted, in bytes.
    pub const MAX_TOML_BYTES: usize = 64 * 1024;

    /// Builds a manifest for the host's own [`API_VERSION`].
    pub fn new(
        name: Name,
        version: &str,
        sha256: Sha256Digest,
        capabilities: impl IntoIterator<Item = Capability>,
    ) -> Result<Self, PluginError> {
        validate_plugin_version(version)?;
        Ok(Self {
            name,
            version: version.to_string(),
            api_version: API_VERSION,
            sha256,
            capabilities: capabilities.into_iter().collect(),
        })
    }

    /// Parses and validates manifest TOML.
    pub fn from_toml(text: &str) -> Result<Self, PluginError> {
        if text.len() > Self::MAX_TOML_BYTES {
            return Err(PluginError::InvalidManifest(format!(
                "manifest is larger than {} bytes",
                Self::MAX_TOML_BYTES
            )));
        }
        let raw: RawManifest = toml::from_str(text)
            .map_err(|e| PluginError::InvalidManifest(sanitize(e.message(), 256)))?;
        let name = Name::new(raw.name).map_err(|e| {
            PluginError::InvalidManifest(format!("name: {}", sanitize(&e.to_string(), 160)))
        })?;
        validate_plugin_version(&raw.version)?;
        let api_version = raw.api_version.parse()?;
        let sha256 = raw.sha256.parse()?;
        let mut capabilities = BTreeSet::new();
        for capability in raw.capabilities {
            if !capabilities.insert(capability) {
                return Err(PluginError::InvalidManifest(format!(
                    "capability `{capability}` is listed twice"
                )));
            }
        }
        Ok(Self {
            name,
            version: raw.version,
            api_version,
            sha256,
            capabilities,
        })
    }

    /// Plugin name.
    pub fn name(&self) -> &Name {
        &self.name
    }

    /// Plugin version (semver).
    pub fn version(&self) -> &str {
        &self.version
    }

    /// Plugin API version the component was built against.
    pub fn api_version(&self) -> ApiVersion {
        self.api_version
    }

    /// Pinned SHA-256 of the component bytes.
    pub fn sha256(&self) -> Sha256Digest {
        self.sha256
    }

    /// Requested capabilities.
    pub fn capabilities(&self) -> &BTreeSet<Capability> {
        &self.capabilities
    }

    /// Whether the manifest requests `capability`.
    pub fn requests(&self, capability: Capability) -> bool {
        self.capabilities.contains(&capability)
    }
}

/// Accepts semver `MAJOR.MINOR.PATCH[-PRERELEASE][+BUILD]`, at most 64 bytes.
pub(crate) fn validate_plugin_version(version: &str) -> Result<(), PluginError> {
    let invalid = || {
        PluginError::InvalidManifest(format!(
            "version `{}` must be semver MAJOR.MINOR.PATCH[-PRE][+BUILD]",
            sanitize(version, 64)
        ))
    };
    if version.is_empty() || version.len() > 64 {
        return Err(invalid());
    }
    let (rest, build) = match version.split_once('+') {
        Some((rest, build)) => (rest, Some(build)),
        None => (version, None),
    };
    let (core, pre) = match rest.split_once('-') {
        Some((core, pre)) => (core, Some(pre)),
        None => (rest, None),
    };
    let numbers: Vec<&str> = core.split('.').collect();
    let numeric_ok = numbers.len() == 3
        && numbers.iter().all(|n| {
            !n.is_empty()
                && n.bytes().all(|b| b.is_ascii_digit())
                && !(n.len() > 1 && n.starts_with('0'))
        });
    let ident_ok = |s: &str| {
        s.split('.').all(|part| {
            !part.is_empty() && part.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
        })
    };
    if numeric_ok && pre.is_none_or(ident_ok) && build.is_none_or(ident_ok) {
        Ok(())
    } else {
        Err(invalid())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const DIGEST: &str = "495cd092b265e860dd5ce72f1346633112293ac05835ed13ba003ef993d7569e";

    fn toml(extra: &str) -> String {
        format!(
            "name = \"toy-endpoints\"\nversion = \"0.1.0\"\napi-version = \"0.1.0\"\n\
             sha256 = \"{DIGEST}\"\n{extra}"
        )
    }

    #[test]
    fn parses_a_minimal_manifest() {
        let m = Manifest::from_toml(&toml("")).unwrap();
        assert_eq!(m.name().as_str(), "toy-endpoints");
        assert_eq!(m.version(), "0.1.0");
        assert_eq!(m.api_version(), ApiVersion::new(0, 1, 0));
        assert_eq!(m.sha256().to_string(), DIGEST);
        assert!(m.capabilities().is_empty());
        assert!(!m.requests(Capability::ProjectFiles));
    }

    #[test]
    fn parses_capabilities() {
        let m = Manifest::from_toml(&toml("capabilities = [\"project-files\"]")).unwrap();
        assert!(m.requests(Capability::ProjectFiles));
    }

    #[test]
    fn rejects_bad_manifests() {
        let cases = [
            toml("capabilities = [\"network\"]"),
            toml("capabilities = [\"project-files\", \"project-files\"]"),
            toml("unknown = 1"),
            toml("").replace("toy-endpoints", "Toy Endpoints"),
            toml("").replace("version = \"0.1.0\"", "version = \"latest\""),
            toml("").replace("api-version = \"0.1.0\"", "api-version = \"0.1\""),
            toml("").replace("api-version = \"0.1.0\"", "api-version = \"0.01.0\""),
            toml("").replace(DIGEST, "abc"),
            toml("").replace(DIGEST, &"g".repeat(64)),
            "name = \"x\"".to_string(),
            "this is not toml = = =".to_string(),
            String::new(),
            "a".repeat(Manifest::MAX_TOML_BYTES + 1),
        ];
        for case in cases {
            assert!(
                matches!(
                    Manifest::from_toml(&case),
                    Err(PluginError::InvalidManifest(_))
                ),
                "accepted: {}",
                case.get(..80).unwrap_or(&case)
            );
        }
    }

    #[test]
    fn digest_accepts_uppercase_and_displays_lowercase() {
        let upper: Sha256Digest = DIGEST.to_uppercase().parse().unwrap();
        assert_eq!(upper.to_string(), DIGEST);
        assert_eq!(
            Sha256Digest::of(b"abc").to_string(),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }

    #[test]
    fn api_version_compatibility() {
        let host = ApiVersion::new(0, 1, 2);
        assert!(ApiVersion::new(0, 1, 0).is_compatible_with(host));
        assert!(ApiVersion::new(0, 1, 2).is_compatible_with(host));
        assert!(
            !ApiVersion::new(0, 1, 3).is_compatible_with(host),
            "newer patch"
        );
        assert!(!ApiVersion::new(0, 2, 0).is_compatible_with(host));
        assert!(!ApiVersion::new(0, 0, 9).is_compatible_with(host));
        assert!(!ApiVersion::new(1, 1, 0).is_compatible_with(host));
        let v1 = ApiVersion::new(1, 4, 0);
        assert!(ApiVersion::new(1, 0, 0).is_compatible_with(v1));
        assert!(!ApiVersion::new(1, 5, 0).is_compatible_with(v1));
        assert!(!ApiVersion::new(2, 0, 0).is_compatible_with(v1));
    }

    #[test]
    fn api_version_parsing_is_strict() {
        assert_eq!(
            "10.2.3".parse::<ApiVersion>().unwrap(),
            ApiVersion::new(10, 2, 3)
        );
        for bad in [
            "",
            "1",
            "1.2",
            "1.2.3.4",
            "1.2.x",
            "01.2.3",
            "1.2.3-pre",
            " 1.2.3",
            "99999999999.0.0",
        ] {
            assert!(bad.parse::<ApiVersion>().is_err(), "{bad}");
        }
    }

    #[test]
    fn plugin_versions() {
        for ok in ["0.1.0", "1.2.3-alpha.1", "1.2.3+build.5", "1.2.3-rc-1+x"] {
            assert!(validate_plugin_version(ok).is_ok(), "{ok}");
        }
        for bad in [
            "",
            "1.2",
            "v1.2.3",
            "1.2.3-",
            "1.2.3+",
            "1.2.3-a..b",
            "01.2.3",
            "1.2.3 ",
        ] {
            assert!(validate_plugin_version(bad).is_err(), "{bad}");
        }
        assert!(validate_plugin_version(&format!("1.2.3-{}", "a".repeat(64))).is_err());
    }

    #[test]
    fn wit_package_matches_api_version() {
        assert!(WIT.contains(&format!("package knowell:plugin@{API_VERSION};")));
    }
}
