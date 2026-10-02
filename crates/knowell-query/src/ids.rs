use std::fmt;

use knowell_core::{ContentHash, LineRange, Name, RepoPath};
use serde::{Deserialize, Serialize};

use crate::QueryError;

/// Identifier of a view inside a project: a tracked branch view, a release
/// view, or a personal worktree layer (`development`, `release/2.x`,
/// `wt-feature-payment`).
///
/// Opaque to the query engine; 1-200 characters without control characters.
#[derive(Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct ViewId(String);

impl ViewId {
    /// Maximum length in characters.
    pub const MAX_LEN: usize = 200;

    /// Validates and wraps `value`.
    pub fn new(value: impl Into<String>) -> Result<Self, QueryError> {
        let value = value.into();
        let len = value.chars().count();
        if len == 0 || len > Self::MAX_LEN || value.chars().any(char::is_control) {
            return Err(QueryError::InvalidViewId);
        }
        Ok(Self(value))
    }

    /// The identifier as text.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl TryFrom<String> for ViewId {
    type Error = QueryError;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        Self::new(value)
    }
}

impl From<ViewId> for String {
    fn from(value: ViewId) -> Self {
        value.0
    }
}

impl fmt::Display for ViewId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl fmt::Debug for ViewId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "ViewId({})", self.0)
    }
}

/// A full git commit id: 40 (SHA-1) or 64 (SHA-256) lowercase hex digits.
#[derive(Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct CommitId(String);

impl CommitId {
    /// Validates and wraps `value`.
    pub fn new(value: impl Into<String>) -> Result<Self, QueryError> {
        let value = value.into();
        let full_length = value.len() == 40 || value.len() == 64;
        let hex = value
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b));
        if full_length && hex {
            Ok(Self(value))
        } else {
            Err(QueryError::InvalidCommit)
        }
    }

    /// The full id.
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// The first 12 hex digits, for labels.
    pub fn short(&self) -> &str {
        self.0.get(..12).unwrap_or(&self.0)
    }
}

impl TryFrom<String> for CommitId {
    type Error = QueryError;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        Self::new(value)
    }
}

impl From<CommitId> for String {
    fn from(value: CommitId) -> Self {
        value.0
    }
}

impl fmt::Display for CommitId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl fmt::Debug for CommitId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "CommitId({})", self.short())
    }
}

/// A programming or document language name, normalised to lowercase
/// (`rust`, `typescript`, `c++`, `c#`, `markdown`).
#[derive(Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct Language(String);

impl Language {
    /// Maximum length in characters.
    pub const MAX_LEN: usize = 64;

    /// Lowercases, validates and wraps `value`.
    pub fn new(value: impl AsRef<str>) -> Result<Self, QueryError> {
        let value = value.as_ref().trim().to_ascii_lowercase();
        let allowed = |c: char| {
            c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, '+' | '#' | '.' | '-' | '_')
        };
        if value.is_empty() || value.len() > Self::MAX_LEN || !value.chars().all(allowed) {
            return Err(QueryError::InvalidLanguage);
        }
        Ok(Self(value))
    }

    /// The normalised name.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl TryFrom<String> for Language {
    type Error = QueryError;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        Self::new(value)
    }
}

impl From<Language> for String {
    fn from(value: Language) -> Self {
        value.0
    }
}

impl fmt::Display for Language {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl fmt::Debug for Language {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Language({})", self.0)
    }
}

/// Which layer of a project a piece of evidence comes from.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Layer {
    /// The project's tracked (shared) view.
    Base,
    /// The user's personal worktree layer, which shadows the base view.
    Overlay,
}

/// Where a piece of evidence lives, precisely enough to fetch the same bytes
/// again: project, view and index generation, path, line range and the hash
/// of the file version the range refers to.
///
/// Field order defines the derived ordering used as the final deterministic
/// tie-break everywhere (project, then path, then range, …).
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct Location {
    /// Project the file belongs to.
    pub project: Name,
    /// Path relative to the project's source root.
    pub path: RepoPath,
    /// 1-based inclusive line span; `None` for a file-level hit.
    pub range: Option<LineRange>,
    /// View the evidence was read from.
    pub view: ViewId,
    /// Index generation of that view the evidence belongs to.
    pub generation: u64,
    /// BLAKE3 hash of the file version (blob) the range refers to.
    pub content_hash: ContentHash,
}

impl Location {
    /// Whether both locations are the same file version in the same view.
    pub fn same_file(&self, other: &Location) -> bool {
        self.project == other.project
            && self.view == other.view
            && self.generation == other.generation
            && self.path == other.path
            && self.content_hash == other.content_hash
    }

    /// Whether the two locations show overlapping text: identical file content
    /// (same content hash) and overlapping line ranges. A file-level location
    /// (no range) overlaps only other file-level locations.
    pub fn overlaps(&self, other: &Location) -> bool {
        if self.content_hash != other.content_hash {
            return false;
        }
        match (self.range, other.range) {
            (Some(a), Some(b)) => a.overlaps(&b),
            (None, None) => true,
            _ => false,
        }
    }

    /// Stable, human-readable identity:
    /// `project@view#generation:path:L10-L20@hash12`.
    pub fn label(&self) -> String {
        let range = self
            .range
            .map_or_else(|| "file".to_owned(), |r| r.to_string());
        format!(
            "{}@{}#{}:{}:{}@{}",
            self.project,
            self.view,
            self.generation,
            self.path,
            range,
            self.content_hash.short()
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn view_id_validates() {
        assert!(ViewId::new("release/2.x").is_ok());
        assert_eq!(ViewId::new(""), Err(QueryError::InvalidViewId));
        assert_eq!(ViewId::new("a\nb"), Err(QueryError::InvalidViewId));
        assert_eq!(ViewId::new("x".repeat(201)), Err(QueryError::InvalidViewId));
        assert!(serde_json::from_str::<ViewId>("\"\"").is_err());
    }

    #[test]
    fn commit_validates() {
        let c = CommitId::new("a".repeat(40)).unwrap();
        assert_eq!(c.short().len(), 12);
        assert!(CommitId::new("abc").is_err());
        assert!(CommitId::new("A".repeat(40)).is_err());
    }

    #[test]
    fn language_normalises() {
        assert_eq!(
            Language::new(" TypeScript ").unwrap().as_str(),
            "typescript"
        );
        assert_eq!(Language::new("C#").unwrap().as_str(), "c#");
        assert!(Language::new("").is_err());
        assert!(Language::new("type script").is_err());
    }

    #[test]
    fn overlap_rules() {
        let base = Location {
            project: Name::new("api").unwrap(),
            path: RepoPath::new("a.rs").unwrap(),
            range: Some(LineRange::new(1, 10).unwrap()),
            view: ViewId::new("main").unwrap(),
            generation: 1,
            content_hash: ContentHash::of(b"a"),
        };
        let mut other = base.clone();
        other.range = Some(LineRange::new(10, 20).unwrap());
        assert!(base.overlaps(&other));
        other.range = None;
        assert!(
            !base.overlaps(&other),
            "file-level only overlaps file-level"
        );
        let mut file = base.clone();
        file.range = None;
        assert!(file.overlaps(&other));
        other.content_hash = ContentHash::of(b"b");
        assert!(!file.overlaps(&other));
        assert!(base.label().starts_with("api@main#1:a.rs:L1-L10@"));
    }
}
