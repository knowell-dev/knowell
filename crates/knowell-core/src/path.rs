use std::fmt;
use std::path::{Component, Path};
use std::str::FromStr;

use schemars::{JsonSchema, Schema, SchemaGenerator};
use serde::{Deserialize, Deserializer, Serialize, Serializer};

/// A normalised path relative to a source root (repository or project root).
///
/// Invariants: non-empty, `/`-separated on every platform, no leading `/`,
/// no drive prefix, no `.` / `..` / empty components, no NUL and no
/// backslash. Comparison is byte-wise; paths are stored exactly as git
/// would report them.
#[derive(Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct RepoPath(String);

/// Error returned when text or an OS path is not a valid [`RepoPath`].
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum RepoPathError {
    /// The path is empty.
    #[error("path must not be empty")]
    Empty,
    /// The path is absolute or carries a drive / UNC prefix.
    #[error("path `{0}` must be relative to the source root")]
    Absolute(String),
    /// The path contains `.`, `..` or an empty component.
    #[error("path `{0}` must not contain `.`, `..` or empty components")]
    NotNormal(String),
    /// The path contains a backslash, NUL or is not valid UTF-8.
    #[error("path `{0}` contains a character that is not allowed")]
    InvalidCharacter(String),
}

impl RepoPath {
    /// Validates a `/`-separated relative path.
    pub fn new(value: impl Into<String>) -> Result<Self, RepoPathError> {
        let value = value.into();
        if value.is_empty() {
            return Err(RepoPathError::Empty);
        }
        if value.contains('\\') || value.contains('\0') {
            return Err(RepoPathError::InvalidCharacter(value));
        }
        if value.starts_with('/') || has_drive_prefix(&value) {
            return Err(RepoPathError::Absolute(value));
        }
        if value
            .split('/')
            .any(|part| part.is_empty() || part == "." || part == "..")
        {
            return Err(RepoPathError::NotNormal(value));
        }
        Ok(Self(value))
    }

    /// Converts a relative OS path (e.g. from a directory walk after
    /// stripping the root) into a [`RepoPath`].
    pub fn from_relative(path: &Path) -> Result<Self, RepoPathError> {
        let mut parts = Vec::new();
        for component in path.components() {
            match component {
                Component::Normal(part) => match part.to_str() {
                    Some(s) => parts.push(s),
                    None => {
                        return Err(RepoPathError::InvalidCharacter(
                            path.to_string_lossy().into_owned(),
                        ));
                    }
                },
                Component::Prefix(_) | Component::RootDir => {
                    return Err(RepoPathError::Absolute(path.to_string_lossy().into_owned()));
                }
                Component::CurDir | Component::ParentDir => {
                    return Err(RepoPathError::NotNormal(
                        path.to_string_lossy().into_owned(),
                    ));
                }
            }
        }
        Self::new(parts.join("/"))
    }

    /// The path as `/`-separated text.
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// The final component (`src/lib.rs` → `lib.rs`).
    pub fn file_name(&self) -> &str {
        self.0.rsplit('/').next().unwrap_or(&self.0)
    }

    /// The extension of the final component without the dot, if any.
    /// Dotfiles such as `.env` have no extension.
    pub fn extension(&self) -> Option<&str> {
        let name = self.file_name();
        let (stem, ext) = name.rsplit_once('.')?;
        if stem.is_empty() || ext.is_empty() {
            None
        } else {
            Some(ext)
        }
    }

    /// Components from root to leaf.
    pub fn components(&self) -> impl DoubleEndedIterator<Item = &str> {
        self.0.split('/')
    }

    /// The parent directory, or `None` for a top-level entry.
    pub fn parent(&self) -> Option<RepoPath> {
        self.0
            .rsplit_once('/')
            .map(|(parent, _)| RepoPath(parent.to_owned()))
    }

    /// Joins a relative child path (`/`-separated) onto this path.
    pub fn join(&self, child: &str) -> Result<RepoPath, RepoPathError> {
        RepoPath::new(format!("{}/{}", self.0, child))
    }
}

fn has_drive_prefix(value: &str) -> bool {
    let bytes = value.as_bytes();
    matches!(bytes, [letter, b':', ..] if letter.is_ascii_alphabetic())
}

impl fmt::Display for RepoPath {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl fmt::Debug for RepoPath {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "RepoPath({})", self.0)
    }
}

impl FromStr for RepoPath {
    type Err = RepoPathError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::new(s)
    }
}

impl AsRef<str> for RepoPath {
    fn as_ref(&self) -> &str {
        &self.0
    }
}

impl Serialize for RepoPath {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.0)
    }
}

impl<'de> Deserialize<'de> for RepoPath {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let s = String::deserialize(deserializer)?;
        Self::new(s).map_err(serde::de::Error::custom)
    }
}

impl JsonSchema for RepoPath {
    fn schema_name() -> std::borrow::Cow<'static, str> {
        "RepoPath".into()
    }

    fn json_schema(_: &mut SchemaGenerator) -> Schema {
        crate::schema::string_schema("Repository-relative path, '/'-separated.", None, &[])
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;

    #[test]
    fn accepts_normal_paths() {
        let p = RepoPath::new("src/billing/service.ts").unwrap();
        assert_eq!(p.file_name(), "service.ts");
        assert_eq!(p.extension(), Some("ts"));
        assert_eq!(p.parent().unwrap().as_str(), "src/billing");
        assert_eq!(
            p.components().collect::<Vec<_>>(),
            ["src", "billing", "service.ts"]
        );
        assert!(RepoPath::new("README").unwrap().parent().is_none());
    }

    #[test]
    fn dotfiles_have_no_extension() {
        assert_eq!(RepoPath::new(".env").unwrap().extension(), None);
        assert_eq!(
            RepoPath::new("a/.env.local").unwrap().extension(),
            Some("local")
        );
        assert_eq!(RepoPath::new("Makefile").unwrap().extension(), None);
    }

    #[test]
    fn rejects_bad_paths() {
        assert_eq!(RepoPath::new(""), Err(RepoPathError::Empty));
        for abs in ["/etc/passwd", "C:/x", "c:x"] {
            assert!(
                matches!(RepoPath::new(abs), Err(RepoPathError::Absolute(_))),
                "{abs}"
            );
        }
        for bad in ["a//b", "./a", "a/../b", "a/", "..", "a/."] {
            assert!(
                matches!(RepoPath::new(bad), Err(RepoPathError::NotNormal(_))),
                "{bad}"
            );
        }
        for bad in ["a\\b", "a\0b"] {
            assert!(matches!(
                RepoPath::new(bad),
                Err(RepoPathError::InvalidCharacter(_))
            ));
        }
    }

    #[test]
    fn converts_os_paths() {
        let rel: PathBuf = ["web", "src", "app.ts"].iter().collect();
        assert_eq!(
            RepoPath::from_relative(&rel).unwrap().as_str(),
            "web/src/app.ts"
        );
        assert!(RepoPath::from_relative(Path::new("../x")).is_err());
    }

    #[test]
    fn join() {
        let p = RepoPath::new("web").unwrap();
        assert_eq!(p.join("src/a.ts").unwrap().as_str(), "web/src/a.ts");
        assert!(p.join("../a").is_err());
    }
}
