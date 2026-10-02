//! The versioned-file resource: `knowell://{workspace}/{project}/{view}/{path}`.
//!
//! `view` is a track target in its text form (`branch:main`, `tag:v2.1.0`,
//! `commit:<sha>`, `worktree`) with `/` percent-encoded (`branch:feature%2Fx`);
//! `path` is the file path relative to the project root, with `/` kept
//! literally. An optional fragment `#L10` or `#L10-L20` selects lines. Every
//! component is percent-decoded and then validated with the core types, so
//! `..`, absolute paths, NUL bytes and invalid UTF-8 are rejected.

use std::fmt;

use knowell_core::{LineRange, Name, RepoPath, TrackTarget};

/// RFC 6570 template advertised by `resources/templates/list`.
pub const FILE_URI_TEMPLATE: &str = "knowell://{workspace}/{project}/{view}/{path}";

const SCHEME: &str = "knowell://";

/// Longest accepted URI, in bytes.
pub const MAX_URI_BYTES: usize = 8192;

/// A parsed versioned-file URI.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct FileUri {
    /// Workspace.
    pub workspace: Name,
    /// Project.
    pub project: Name,
    /// View (ref) to read the file at.
    pub view: TrackTarget,
    /// File path relative to the project root.
    pub path: RepoPath,
    /// Lines to read; the whole file when absent.
    pub lines: Option<LineRange>,
}

/// Why a URI is not a valid versioned-file URI. Messages never echo the URI.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum UriError {
    /// The URI does not start with `knowell://`.
    #[error("resource uri must start with `knowell://`")]
    Scheme,
    /// The URI is longer than [`MAX_URI_BYTES`].
    #[error("resource uri is longer than {MAX_URI_BYTES} bytes")]
    TooLong,
    /// A component is missing.
    #[error(
        "resource uri is missing the {0} component; expected knowell://{{workspace}}/{{project}}/{{view}}/{{path}}"
    )]
    Missing(&'static str),
    /// A percent-escape is malformed or decodes to invalid UTF-8.
    #[error("resource uri has a malformed percent-escape in the {0} component")]
    Encoding(&'static str),
    /// A component decodes to an invalid value.
    #[error("resource uri has an invalid {0} component")]
    Invalid(&'static str),
    /// Query strings are not supported.
    #[error("resource uri must not contain a query string")]
    Query,
}

impl FileUri {
    /// Parses and validates a versioned-file URI.
    pub fn parse(uri: &str) -> Result<Self, UriError> {
        if uri.len() > MAX_URI_BYTES {
            return Err(UriError::TooLong);
        }
        let rest = uri.strip_prefix(SCHEME).ok_or(UriError::Scheme)?;
        if rest.contains('?') {
            return Err(UriError::Query);
        }
        let (rest, fragment) = match rest.split_once('#') {
            Some((rest, fragment)) => (rest, Some(fragment)),
            None => (rest, None),
        };
        let mut parts = rest.splitn(4, '/');
        let workspace = non_empty(parts.next(), "workspace")?;
        let project = non_empty(parts.next(), "project")?;
        let view = non_empty(parts.next(), "view")?;
        let path = non_empty(parts.next(), "path")?;

        let workspace = Name::new(decode(workspace, "workspace")?)
            .map_err(|_| UriError::Invalid("workspace"))?;
        let project =
            Name::new(decode(project, "project")?).map_err(|_| UriError::Invalid("project"))?;
        let view: TrackTarget = decode(view, "view")?
            .parse()
            .map_err(|_| UriError::Invalid("view"))?;
        let path = RepoPath::new(decode(path, "path")?).map_err(|_| UriError::Invalid("path"))?;
        let lines = fragment.map(parse_line_fragment).transpose()?;
        Ok(Self {
            workspace,
            project,
            view,
            path,
            lines,
        })
    }
}

impl fmt::Display for FileUri {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{SCHEME}{}/{}/{}/{}",
            self.workspace,
            self.project,
            encode(&self.view.to_string(), false),
            encode(self.path.as_str(), true)
        )?;
        if let Some(lines) = self.lines {
            write!(f, "#{lines}")?;
        }
        Ok(())
    }
}

fn non_empty<'a>(part: Option<&'a str>, component: &'static str) -> Result<&'a str, UriError> {
    match part {
        Some(p) if !p.is_empty() => Ok(p),
        _ => Err(UriError::Missing(component)),
    }
}

/// `L10` or `L10-L20` (the `Display` form of [`LineRange`]).
pub(crate) fn parse_line_fragment(fragment: &str) -> Result<LineRange, UriError> {
    let invalid = || UriError::Invalid("line fragment");
    let number = |text: &str| -> Result<u32, UriError> {
        let digits = text.strip_prefix('L').ok_or_else(invalid)?;
        if digits.is_empty() || digits.len() > 9 || !digits.bytes().all(|b| b.is_ascii_digit()) {
            return Err(invalid());
        }
        digits.parse().map_err(|_| invalid())
    };
    let (start, end) = match fragment.split_once('-') {
        Some((start, end)) => (number(start)?, number(end)?),
        None => {
            let line = number(fragment)?;
            (line, line)
        }
    };
    LineRange::new(start, end).map_err(|_| invalid())
}

fn decode(text: &str, component: &'static str) -> Result<String, UriError> {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0usize;
    while let Some(&byte) = bytes.get(i) {
        if byte == b'%' {
            let hi = bytes.get(i.saturating_add(1)).copied().and_then(hex_value);
            let lo = bytes.get(i.saturating_add(2)).copied().and_then(hex_value);
            let (Some(hi), Some(lo)) = (hi, lo) else {
                return Err(UriError::Encoding(component));
            };
            out.push((hi << 4) | lo);
            i = i.saturating_add(3);
        } else {
            out.push(byte);
            i = i.saturating_add(1);
        }
    }
    String::from_utf8(out).map_err(|_| UriError::Encoding(component))
}

fn hex_value(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

/// Percent-encodes everything except RFC 3986 unreserved characters and
/// `:` (and `/` when `keep_slash`).
fn encode(text: &str, keep_slash: bool) -> String {
    const HEX: &[u8; 16] = b"0123456789ABCDEF";
    let mut out = String::with_capacity(text.len());
    for &byte in text.as_bytes() {
        let keep = byte.is_ascii_alphanumeric()
            || matches!(byte, b'-' | b'.' | b'_' | b'~' | b':')
            || (keep_slash && byte == b'/');
        if keep {
            out.push(char::from(byte));
        } else {
            out.push('%');
            out.push(char::from(
                HEX.get(usize::from(byte >> 4)).copied().unwrap_or(b'0'),
            ));
            out.push(char::from(
                HEX.get(usize::from(byte & 0x0f)).copied().unwrap_or(b'0'),
            ));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_and_round_trips() {
        let uri = "knowell://demo-shop/billing-api/branch:feature%2Fretry/src/payments/payment.service.ts#L12-L40";
        let parsed = FileUri::parse(uri).unwrap();
        assert_eq!(parsed.workspace.as_str(), "demo-shop");
        assert_eq!(parsed.project.as_str(), "billing-api");
        assert_eq!(parsed.view.to_string(), "branch:feature/retry");
        assert_eq!(parsed.path.as_str(), "src/payments/payment.service.ts");
        assert_eq!(parsed.lines, Some(LineRange::new(12, 40).unwrap()));
        assert_eq!(parsed.to_string(), uri);

        let simple = FileUri::parse("knowell://w/p/worktree/README.md").unwrap();
        assert_eq!(simple.view, TrackTarget::WorktreeHead);
        assert_eq!(simple.lines, None);
        assert_eq!(simple.to_string(), "knowell://w/p/worktree/README.md");

        let encoded = FileUri::parse("knowell://w/p/tag:v1/docs%2Fa%20b.md#L7").unwrap();
        assert_eq!(encoded.path.as_str(), "docs/a b.md");
        assert_eq!(encoded.to_string(), "knowell://w/p/tag:v1/docs/a%20b.md#L7");
    }

    #[test]
    fn rejects_malformed_and_hostile_uris() {
        let cases = [
            ("file:///etc/passwd", UriError::Scheme),
            ("knowell://", UriError::Missing("workspace")),
            ("knowell://w", UriError::Missing("project")),
            ("knowell://w/p", UriError::Missing("view")),
            ("knowell://w/p/branch:main", UriError::Missing("path")),
            ("knowell://w/p/branch:main/", UriError::Missing("path")),
            (
                "knowell://W/p/branch:main/a",
                UriError::Invalid("workspace"),
            ),
            ("knowell://w/p/main/a", UriError::Invalid("view")),
            (
                "knowell://w/p/branch:main/../etc/passwd",
                UriError::Invalid("path"),
            ),
            (
                "knowell://w/p/branch:main/%2E%2E/x",
                UriError::Invalid("path"),
            ),
            (
                "knowell://w/p/branch:main//etc/passwd",
                UriError::Invalid("path"),
            ),
            ("knowell://w/p/branch:main/a%00b", UriError::Invalid("path")),
            ("knowell://w/p/branch:main/a%5Cb", UriError::Invalid("path")),
            ("knowell://w/p/branch:main/a%zz", UriError::Encoding("path")),
            ("knowell://w/p/branch:main/a%2", UriError::Encoding("path")),
            ("knowell://w/p/branch:main/a%ff", UriError::Encoding("path")),
            ("knowell://w/p/branch:main/a?x=1", UriError::Query),
            (
                "knowell://w/p/branch:main/a#L0",
                UriError::Invalid("line fragment"),
            ),
            (
                "knowell://w/p/branch:main/a#L5-L2",
                UriError::Invalid("line fragment"),
            ),
            (
                "knowell://w/p/branch:main/a#12",
                UriError::Invalid("line fragment"),
            ),
            (
                "knowell://w/p/branch:main/a#L99999999999",
                UriError::Invalid("line fragment"),
            ),
        ];
        for (uri, expected) in cases {
            let error = FileUri::parse(uri).unwrap_err();
            assert_eq!(error, expected, "{uri}");
            assert!(
                !error.to_string().contains("passwd"),
                "error echoes the uri"
            );
        }
        let long = format!("knowell://w/p/branch:main/{}", "a".repeat(MAX_URI_BYTES));
        assert_eq!(FileUri::parse(&long).unwrap_err(), UriError::TooLong);
    }

    #[test]
    fn encodes_reserved_characters() {
        assert_eq!(encode("branch:feature/x", false), "branch:feature%2Fx");
        assert_eq!(encode("a b/ü.rs", true), "a%20b/%C3%BC.rs");
        assert_eq!(decode("a%20b/%C3%BC.rs", "path").unwrap(), "a b/ü.rs");
    }
}
