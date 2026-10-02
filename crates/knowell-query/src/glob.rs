use std::fmt;

use knowell_core::RepoPath;
use serde::{Deserialize, Serialize};

use crate::QueryError;
use crate::error::echo;

/// A path pattern matched against [`RepoPath`]s.
///
/// Syntax (deliberately small, no character classes or braces):
///
/// | Pattern | Matches |
/// |---|---|
/// | `*` | any characters inside one path segment |
/// | `?` | exactly one character inside one path segment |
/// | `**` (a whole segment) | zero or more whole segments |
/// | `dir/` (trailing slash) | everything below `dir` (same as `dir/**`) |
/// | no `/` at all (`*.ts`) | the file name at any depth (same as `**/*.ts`) |
///
/// Matching is case-sensitive and byte-exact, like git paths. Patterns are
/// relative: a leading `/`, `.` / `..` segments and backslashes are rejected.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct PathGlob {
    pattern: String,
    segments: Vec<Segment>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum Segment {
    AnyDepth,
    Pattern(Vec<GlobChar>),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum GlobChar {
    Any,
    One,
    Literal(char),
}

impl PathGlob {
    /// Maximum pattern length in characters.
    pub const MAX_LEN: usize = 512;

    /// Parses `pattern`.
    pub fn new(pattern: impl Into<String>) -> Result<Self, QueryError> {
        let pattern = pattern.into();
        let invalid = |reason| QueryError::InvalidGlob {
            pattern: echo(&pattern),
            reason,
        };
        if pattern.is_empty() {
            return Err(invalid("pattern is empty"));
        }
        if pattern.chars().count() > Self::MAX_LEN {
            return Err(invalid("pattern is longer than 512 characters"));
        }
        if pattern.contains('\\') || pattern.chars().any(char::is_control) {
            return Err(invalid("pattern contains a backslash or control character"));
        }
        if pattern.starts_with('/') {
            return Err(invalid("pattern must be relative to the source root"));
        }
        let body = pattern.strip_suffix('/').unwrap_or(&pattern);
        let mut segments = Vec::new();
        if !body.contains('/') && !pattern.ends_with('/') {
            segments.push(Segment::AnyDepth);
        }
        for part in body.split('/') {
            if part.is_empty() || part == "." || part == ".." {
                return Err(invalid(
                    "pattern must not contain empty, `.` or `..` segments",
                ));
            }
            if part == "**" {
                // Consecutive `**` segments are equivalent to one.
                if segments.last() != Some(&Segment::AnyDepth) {
                    segments.push(Segment::AnyDepth);
                }
                continue;
            }
            let mut chars = Vec::new();
            for c in part.chars() {
                let glob_char = match c {
                    '*' => GlobChar::Any,
                    '?' => GlobChar::One,
                    other => GlobChar::Literal(other),
                };
                // `a**b` inside a segment means the same as `a*b`.
                if glob_char == GlobChar::Any && chars.last() == Some(&GlobChar::Any) {
                    continue;
                }
                chars.push(glob_char);
            }
            segments.push(Segment::Pattern(chars));
        }
        if pattern.ends_with('/') && segments.last() != Some(&Segment::AnyDepth) {
            segments.push(Segment::AnyDepth);
        }
        Ok(Self { pattern, segments })
    }

    /// The pattern as written.
    pub fn as_str(&self) -> &str {
        &self.pattern
    }

    /// Whether `path` matches the pattern.
    pub fn matches(&self, path: &RepoPath) -> bool {
        let parts: Vec<Vec<char>> = path.components().map(|p| p.chars().collect()).collect();
        wildcard_match(
            &self.segments,
            &parts,
            |segment| *segment == Segment::AnyDepth,
            |segment, part| match segment {
                Segment::AnyDepth => false,
                Segment::Pattern(chars) => wildcard_match(
                    chars,
                    part,
                    |c| *c == GlobChar::Any,
                    |c, actual| match c {
                        GlobChar::One => true,
                        GlobChar::Literal(expected) => expected == actual,
                        GlobChar::Any => false,
                    },
                ),
            },
        )
    }
}

/// Greedy wildcard matching with single-star backtracking.
///
/// `is_star` elements match any (possibly empty) sequence of items; every
/// other element matches exactly one item when `matches_one` says so. The
/// greedy algorithm is exact for this pattern class and runs in
/// O(pattern × items) time without recursion, so hostile patterns such as
/// `*a*a*a*a*b` cannot blow up.
fn wildcard_match<P, T>(
    pattern: &[P],
    items: &[T],
    is_star: impl Fn(&P) -> bool,
    matches_one: impl Fn(&P, &T) -> bool,
) -> bool {
    let mut p = 0usize;
    let mut t = 0usize;
    // (pattern index after the last star, item index the star currently ends at)
    let mut backtrack: Option<(usize, usize)> = None;
    loop {
        let element = pattern.get(p);
        if let Some(element) = element
            && is_star(element)
        {
            backtrack = Some((p + 1, t));
            p += 1;
            continue;
        }
        match (element, items.get(t)) {
            (Some(element), Some(item)) if matches_one(element, item) => {
                p += 1;
                t += 1;
            }
            (None, None) => return true,
            (Some(_), None) => return false,
            (_, Some(_)) => {
                let Some((star_next, star_end)) = backtrack else {
                    return false;
                };
                let extended = star_end + 1;
                backtrack = Some((star_next, extended));
                p = star_next;
                t = extended;
            }
        }
    }
}

impl TryFrom<String> for PathGlob {
    type Error = QueryError;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        Self::new(value)
    }
}

impl From<PathGlob> for String {
    fn from(value: PathGlob) -> Self {
        value.pattern
    }
}

impl fmt::Display for PathGlob {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.pattern)
    }
}

impl fmt::Debug for PathGlob {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "PathGlob({})", self.pattern)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn m(pattern: &str, path: &str) -> bool {
        PathGlob::new(pattern)
            .unwrap()
            .matches(&RepoPath::new(path).unwrap())
    }

    #[test]
    fn single_segment_star() {
        assert!(m("src/*.rs", "src/lib.rs"));
        assert!(!m("src/*.rs", "src/a/lib.rs"));
        assert!(m("src/?.rs", "src/a.rs"));
        assert!(!m("src/?.rs", "src/ab.rs"));
    }

    #[test]
    fn any_depth() {
        assert!(m("src/**/*.rs", "src/lib.rs"));
        assert!(m("src/**/*.rs", "src/a/b/c.rs"));
        assert!(m("**/test/**", "a/test/b.rs"));
        assert!(!m("**/test/**", "a/tests/b.rs"));
        assert!(m("src/**", "src/a/b"));
        assert!(m("src/", "src/a/b"));
        assert!(!m("src/", "srcx/a"));
    }

    #[test]
    fn bare_name_matches_at_any_depth() {
        assert!(m("*.ts", "web/src/app.ts"));
        assert!(m("*.ts", "app.ts"));
        assert!(m("Cargo.toml", "crates/x/Cargo.toml"));
        assert!(!m("*.ts", "app.tsx"));
    }

    #[test]
    fn literal_is_case_sensitive() {
        assert!(!m("SRC/*.rs", "src/a.rs"));
    }

    #[test]
    fn rejects_bad_patterns() {
        for bad in ["", "/abs/*.rs", "a//b", "a/../b", "./a", "a\\b", "a\0b"] {
            assert!(PathGlob::new(bad).is_err(), "{bad:?}");
        }
        let long = "a".repeat(513);
        assert!(PathGlob::new(long).is_err());
    }

    #[test]
    fn hostile_pattern_is_fast_and_correct() {
        let pattern = format!("{}b", "*a".repeat(200));
        let path = "a".repeat(400);
        assert!(!m(&pattern, &path));
        let path_b = format!("{}b", "a".repeat(400));
        assert!(m(&pattern, &path_b));
        let deep = vec!["x"; 200].join("/");
        assert!(!m(&"**/y/".repeat(50), &deep));
    }

    #[test]
    fn serde_round_trips() {
        let g: PathGlob = serde_json::from_str("\"src/**\"").unwrap();
        assert_eq!(serde_json::to_string(&g).unwrap(), "\"src/**\"");
        assert!(serde_json::from_str::<PathGlob>("\"/x\"").is_err());
    }
}
