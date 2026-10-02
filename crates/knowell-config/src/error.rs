//! Error types: validation issues and load/parse errors.
//!
//! Parse errors are rendered by this crate rather than by the `toml` crate,
//! whose `Display` quotes the offending source line. If a user pasted a real
//! API key where a secret *reference* belongs, that quote would print it.

use std::fmt;
use std::ops::Range;
use std::path::PathBuf;

/// One problem found while validating or resolving a configuration.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfigIssue {
    /// Location of the problem in the file, e.g. `project[2].track` or
    /// `providers.gemini.api_key`. Empty for whole-file problems.
    pub path: String,
    /// Human-readable explanation. Never contains configuration values that
    /// could be secrets (URLs and reference targets are not echoed).
    pub message: String,
}

impl ConfigIssue {
    /// Creates an issue.
    pub fn new(path: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            path: path.into(),
            message: message.into(),
        }
    }
}

impl fmt::Display for ConfigIssue {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.path.is_empty() {
            f.write_str(&self.message)
        } else {
            write!(f, "{}: {}", self.path, self.message)
        }
    }
}

/// All issues found in one pass; validation never stops at the first problem.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ConfigIssues(pub Vec<ConfigIssue>);

impl ConfigIssues {
    /// Whether no issue was found.
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// Number of issues.
    pub fn len(&self) -> usize {
        self.0.len()
    }

    /// Iterates over the issues in discovery order.
    pub fn iter(&self) -> std::slice::Iter<'_, ConfigIssue> {
        self.0.iter()
    }
}

impl fmt::Display for ConfigIssues {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} configuration problem(s)", self.0.len())?;
        for issue in &self.0 {
            write!(f, "\n  - {issue}")?;
        }
        Ok(())
    }
}

impl std::error::Error for ConfigIssues {}

impl<'a> IntoIterator for &'a ConfigIssues {
    type Item = &'a ConfigIssue;
    type IntoIter = std::slice::Iter<'a, ConfigIssue>;

    fn into_iter(self) -> Self::IntoIter {
        self.0.iter()
    }
}

/// Error returned when loading a configuration file.
#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    /// The file could not be read.
    #[error("cannot read `{}`: {source}", path.display())]
    Read {
        /// File that could not be read.
        path: PathBuf,
        /// Underlying I/O error.
        source: std::io::Error,
    },
    /// The text is not valid TOML or does not match the configuration shape.
    /// The message never quotes source text that could be a secret.
    #[error("{}", render_parse(file, *line, *column, message))]
    Parse {
        /// File name, or a placeholder when parsing from memory.
        file: String,
        /// 1-based line of the problem, when known.
        line: Option<usize>,
        /// 1-based column (in characters) of the problem, when known.
        column: Option<usize>,
        /// Problem description with any echoed value redacted.
        message: String,
    },
    /// The file parsed but violates configuration rules.
    #[error("invalid configuration in `{file}`: {issues}")]
    Invalid {
        /// File name, or a placeholder when parsing from memory.
        file: String,
        /// Every rule violation found.
        issues: ConfigIssues,
    },
}

fn render_parse(file: &str, line: Option<usize>, column: Option<usize>, message: &str) -> String {
    match (line, column) {
        (Some(l), Some(c)) => format!("{file}:{l}:{c}: {message}"),
        _ => format!("{file}: {message}"),
    }
}

impl ConfigError {
    /// Builds a parse error from a `toml` error without ever using its
    /// `Display`, which quotes the source line.
    pub(crate) fn from_toml(file: &str, source: &str, err: &toml::de::Error) -> Self {
        let span = err.span();
        let (line, column) = match &span {
            Some(span) => {
                let (l, c) = line_col(source, span.start);
                (Some(l), Some(c))
            }
            None => (None, None),
        };
        ConfigError::Parse {
            file: file.to_owned(),
            line,
            column,
            message: sanitize(err.message(), source, span),
        }
    }
}

/// Converts a byte offset to a 1-based (line, column in chars) pair.
/// Offsets past the end or inside a character are clamped.
fn line_col(source: &str, offset: usize) -> (usize, usize) {
    let mut end = offset.min(source.len());
    while end > 0 && !source.is_char_boundary(end) {
        end -= 1;
    }
    let prefix = source.get(..end).unwrap_or("");
    let line = prefix.bytes().filter(|b| *b == b'\n').count() + 1;
    let line_start = prefix.rfind('\n').map_or(0, |i| i + 1);
    let column = prefix.get(line_start..).map_or(0, |s| s.chars().count()) + 1;
    (line, column)
}

/// Removes values that serde/toml echo into messages (`invalid type: string
/// "<value>"`, `unknown variant `<value>``) and the offending source slice.
fn sanitize(raw: &str, source: &str, span: Option<Range<usize>>) -> String {
    const MASK: &str = "<redacted>";
    let mut msg = mask_between(raw, "unknown variant `", "`", MASK);
    msg = mask_between(&msg, "string \"", "\", ", MASK);
    if !raw.starts_with("unknown field") && !raw.starts_with("duplicate key") {
        let slice = span
            .and_then(|s| source.get(s))
            .map(|s| s.trim().trim_matches(|c| c == '"' || c == '\''))
            .unwrap_or("");
        if !slice.is_empty() {
            msg = msg.replace(slice, MASK);
        }
    }
    msg
}

/// Replaces the text between `open` and the next `close` (or the end).
fn mask_between(msg: &str, open: &str, close: &str, mask: &str) -> String {
    let Some(start) = msg.find(open) else {
        return msg.to_owned();
    };
    let value_start = start + open.len();
    let rest = msg.get(value_start..).unwrap_or("");
    let value_len = rest.find(close).unwrap_or(rest.len());
    let head = msg.get(..value_start).unwrap_or("");
    let tail = rest.get(value_len..).unwrap_or("");
    format!("{head}{mask}{tail}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn line_and_column_are_one_based() {
        let src = "ab\ncd\u{e9}\nxyz";
        assert_eq!(line_col(src, 0), (1, 1));
        assert_eq!(line_col(src, 1), (1, 2));
        assert_eq!(line_col(src, 3), (2, 1));
        assert_eq!(line_col(src, 8), (3, 1));
        assert_eq!(line_col(src, 999), (3, 4));
        // Inside the two-byte character clamps back to its start.
        assert_eq!(line_col(src, 6), (2, 3));
    }

    #[test]
    fn issues_display_lists_every_issue() {
        let issues = ConfigIssues(vec![
            ConfigIssue::new("a.b", "bad"),
            ConfigIssue::new("", "worse"),
        ]);
        let text = issues.to_string();
        assert!(text.starts_with("2 configuration problem(s)"));
        assert!(text.contains("- a.b: bad"));
        assert!(text.contains("- worse"));
    }

    #[test]
    fn masks_echoed_values() {
        let out = sanitize(
            "unknown variant `SECRETVALUE`, expected one of `a`, `b`",
            "",
            None,
        );
        assert!(!out.contains("SECRETVALUE"));
        assert!(out.contains("expected one of"));
        let out = sanitize(
            "invalid type: string \"SECRETVALUE\", expected u32",
            "",
            None,
        );
        assert!(!out.contains("SECRETVALUE"));
    }
}
