//! Stable result ids: `kn:{project}:{commit12}:{hash16}:{path}#L{a}-L{b}`.
//!
//! An id names an exact file version (the first 16 hex digits of its BLAKE3
//! content hash) and a line range, plus the commit of the view it came from
//! for display. `fetch` resolves it against the context's view: the same
//! version is `current`; another version of the same path is `changed` (with
//! the id of the current range); a missing path is `deleted`.
//!
//! Paths too long for the 512-byte id limit are replaced by `~` and the
//! first 16 hex digits of the path's hash; the engine resolves those against
//! the pinned file list.

use knowell_core::{ContentHash, LineRange, Name, RepoPath};
use knowell_mcp::{ResultId, ToolError};

/// Prefix of source result ids.
const SOURCE_PREFIX: &str = "kn:";
/// Prefix of contract ids.
const CONTRACT_PREFIX: &str = "kn-contract:";
/// Hex digits of the content hash kept in an id.
const HASH_DIGITS: usize = 16;
/// Placeholder commit for sources without one (plain directories).
const NO_COMMIT: &str = "none";

/// How an id names its path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum PathRef {
    /// The path itself.
    Plain(RepoPath),
    /// The first 16 hex digits of the path's BLAKE3 hash.
    Hashed(String),
}

impl PathRef {
    /// Whether `path` is the path this reference names.
    pub(crate) fn matches(&self, path: &RepoPath) -> bool {
        match self {
            PathRef::Plain(p) => p == path,
            PathRef::Hashed(prefix) => path_digest(path) == *prefix,
        }
    }
}

/// A decoded source result id.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SourceRef {
    pub(crate) project: Name,
    pub(crate) commit12: String,
    pub(crate) hash16: String,
    pub(crate) path: PathRef,
    pub(crate) lines: LineRange,
}

impl SourceRef {
    /// Whether `hash` is the file version this id names.
    pub(crate) fn names_version(&self, hash: &ContentHash) -> bool {
        hash_digest(hash) == self.hash16
    }
}

fn hash_digest(hash: &ContentHash) -> String {
    hash.to_string().chars().take(HASH_DIGITS).collect()
}

fn path_digest(path: &RepoPath) -> String {
    hash_digest(&ContentHash::of(path.as_str().as_bytes()))
}

/// The id of `lines` of the file version `hash` at `path`.
pub(crate) fn source_id(
    project: &Name,
    commit: Option<&str>,
    hash: &ContentHash,
    path: &RepoPath,
    lines: LineRange,
) -> Result<ResultId, ToolError> {
    let commit12: String =
        commit.map_or_else(|| NO_COMMIT.to_owned(), |c| c.chars().take(12).collect());
    let hash16 = hash_digest(hash);
    let plain = format!("{SOURCE_PREFIX}{project}:{commit12}:{hash16}:{path}#{lines}");
    let text = if plain.len() <= ResultId::MAX_LEN && is_id_text(&plain) {
        plain
    } else {
        format!(
            "{SOURCE_PREFIX}{project}:{commit12}:{hash16}:~{}#{lines}",
            path_digest(path)
        )
    };
    ResultId::new(text).map_err(|e| ToolError::internal(format!("result id: {e}")))
}

/// Whether every byte is printable ASCII without spaces (the id alphabet).
fn is_id_text(text: &str) -> bool {
    text.bytes().all(|b| b.is_ascii_graphic())
}

/// Decodes a source id; `None` for anything else (contract and memory ids,
/// malformed or hostile input).
pub(crate) fn parse_source_id(id: &ResultId) -> Option<SourceRef> {
    let rest = id.as_str().strip_prefix(SOURCE_PREFIX)?;
    let (project, rest) = rest.split_once(':')?;
    let (commit12, rest) = rest.split_once(':')?;
    let (hash16, rest) = rest.split_once(':')?;
    let (path, lines) = rest.rsplit_once('#')?;
    let project = Name::new(project).ok()?;
    if hash16.len() != HASH_DIGITS || !hash16.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    if commit12 != NO_COMMIT
        && (commit12.is_empty()
            || commit12.len() > 12
            || !commit12.bytes().all(|b| b.is_ascii_hexdigit()))
    {
        return None;
    }
    let path = match path.strip_prefix('~') {
        Some(digest)
            if digest.len() == HASH_DIGITS && digest.bytes().all(|b| b.is_ascii_hexdigit()) =>
        {
            PathRef::Hashed(digest.to_owned())
        }
        Some(_) => return None,
        None => PathRef::Plain(RepoPath::new(path).ok()?),
    };
    Some(SourceRef {
        project,
        commit12: commit12.to_owned(),
        hash16: hash16.to_owned(),
        path,
        lines: parse_lines(lines)?,
    })
}

/// Parses `L12` or `L12-L40` (1-based, inclusive).
pub(crate) fn parse_lines(text: &str) -> Option<LineRange> {
    let number = |s: &str| -> Option<u32> {
        let digits = s.strip_prefix('L')?;
        if digits.is_empty() || digits.len() > 9 || !digits.bytes().all(|b| b.is_ascii_digit()) {
            return None;
        }
        digits.parse().ok()
    };
    match text.split_once('-') {
        Some((a, b)) => LineRange::new(number(a)?, number(b)?).ok(),
        None => {
            let line = number(text)?;
            LineRange::new(line, line).ok()
        }
    }
}

/// The id of a contract record (not a source range).
pub(crate) fn contract_id(kind: &str, key: &str) -> Result<ResultId, ToolError> {
    let readable: String = key
        .chars()
        .map(|c| if c == ' ' { '_' } else { c })
        .collect();
    let plain = format!("{CONTRACT_PREFIX}{kind}:{readable}");
    let text = if plain.len() <= ResultId::MAX_LEN && is_id_text(&plain) {
        plain
    } else {
        format!(
            "{CONTRACT_PREFIX}{kind}:~{}",
            hash_digest(&ContentHash::of(key.as_bytes()))
        )
    };
    ResultId::new(text).map_err(|e| ToolError::internal(format!("contract id: {e}")))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn name(s: &str) -> Name {
        Name::new(s).unwrap()
    }

    #[test]
    fn source_ids_round_trip() {
        let hash = ContentHash::of(b"x");
        let path = RepoPath::new("src/a b/c.ts").unwrap();
        let lines = LineRange::new(3, 9).unwrap();
        let commit = "a".repeat(40);
        let id = source_id(
            &name("api"),
            Some(&commit),
            &hash,
            &RepoPath::new("src/c.ts").unwrap(),
            lines,
        )
        .unwrap();
        assert!(id.as_str().starts_with("kn:api:aaaaaaaaaaaa:"));
        let parsed = parse_source_id(&id).unwrap();
        assert_eq!(parsed.project.as_str(), "api");
        assert_eq!(parsed.lines, lines);
        assert!(parsed.names_version(&hash));
        assert!(parsed.path.matches(&RepoPath::new("src/c.ts").unwrap()));
        // A path with a space cannot appear verbatim: it is hashed.
        let id = source_id(&name("api"), None, &hash, &path, lines).unwrap();
        let parsed = parse_source_id(&id).unwrap();
        assert!(matches!(parsed.path, PathRef::Hashed(_)));
        assert!(parsed.path.matches(&path));
        assert_eq!(parsed.commit12, "none");
    }

    #[test]
    fn long_paths_are_hashed() {
        let long = format!("{}/x.rs", "d".repeat(600));
        let path = RepoPath::new(long).unwrap();
        let id = source_id(
            &name("api"),
            None,
            &ContentHash::of(b"y"),
            &path,
            LineRange::new(1, 1).unwrap(),
        )
        .unwrap();
        assert!(id.as_str().len() <= ResultId::MAX_LEN);
        assert!(parse_source_id(&id).unwrap().path.matches(&path));
    }

    #[test]
    fn hostile_ids_are_rejected() {
        for text in [
            "kn:",
            "kn:api",
            "kn:api:abc:0123456789abcdef:src/a.ts",
            "kn:API:abc:0123456789abcdef:a.ts#L1",
            "kn:api:xyz:0123456789abcdef:a.ts#L1",
            "kn:api:abc:0123:a.ts#L1",
            "kn:api:abc:0123456789abcdef:../a.ts#L1",
            "kn:api:abc:0123456789abcdef:a.ts#L0",
            "kn:api:abc:0123456789abcdef:a.ts#L5-L2",
            "kn:api:abc:0123456789abcdef:a.ts#L99999999999",
            "kn:api:abc:0123456789abcdef:~zz#L1",
            "kn-contract:endpoint:GET_/x",
            "mem-1",
        ] {
            let id = ResultId::new(text).unwrap();
            assert!(parse_source_id(&id).is_none(), "{text}");
        }
    }

    #[test]
    fn contract_ids_are_readable_or_hashed() {
        let id = contract_id("endpoint", "POST /v1/subscriptions/{id}/cancel").unwrap();
        assert_eq!(
            id.as_str(),
            "kn-contract:endpoint:POST_/v1/subscriptions/{id}/cancel"
        );
        let id = contract_id("i18n_key", "anahtar.ödeme").unwrap();
        assert!(id.as_str().starts_with("kn-contract:i18n_key:~"));
    }
}
