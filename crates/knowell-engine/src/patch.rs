//! Unified diffs (`analyze_impact` on an unapplied patch): parsing,
//! applying to a base text, and the changed line ranges per file.
//!
//! The patch is untrusted input: every header is validated, paths must be
//! relative repository paths (no `..`, no absolute or drive paths, no
//! backslashes), counts are bounded, and errors name the patch line, never
//! its content.

use knowell_core::{LineRange, RepoPath};

/// Most files one patch may touch.
pub(crate) const MAX_FILES: usize = 1000;
/// Most hunks per file.
pub(crate) const MAX_HUNKS: usize = 10_000;

/// Why a patch could not be read.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub(crate) enum PatchError {
    /// A header or hunk line is malformed.
    #[error("patch line {line}: {reason}")]
    Malformed {
        /// 1-based line in the patch.
        line: usize,
        /// What is wrong.
        reason: &'static str,
    },
    /// The patch touches no file.
    #[error("the patch contains no file changes")]
    Empty,
    /// Too many files or hunks.
    #[error("the patch is too large: {0}")]
    TooLarge(&'static str),
}

/// What a hunk line does.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum LineKind {
    Context,
    Removed,
    Added,
}

/// One hunk.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Hunk {
    pub(crate) old_start: u32,
    pub(crate) old_len: u32,
    pub(crate) new_start: u32,
    pub(crate) new_len: u32,
    pub(crate) lines: Vec<(LineKind, String)>,
}

impl Hunk {
    /// Lines of the old file the hunk removes or inserts between, as a
    /// range (an insertion touches the line it follows, or line 1).
    pub(crate) fn old_touched(&self) -> Option<LineRange> {
        let mut line = self.old_start;
        let mut first = None;
        let mut last = None;
        for (kind, _) in &self.lines {
            match kind {
                LineKind::Context => line = line.saturating_add(1),
                LineKind::Removed => {
                    first.get_or_insert(line);
                    last = Some(line);
                    line = line.saturating_add(1);
                }
                LineKind::Added => {
                    let at = line.saturating_sub(1).max(1);
                    first.get_or_insert(at);
                    last = Some(last.map_or(at, |l: u32| l.max(at)));
                }
            }
        }
        LineRange::new(first?, last?).ok()
    }
}

/// The changes to one file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct FilePatch {
    /// `None` for an added file.
    pub(crate) old_path: Option<RepoPath>,
    /// `None` for a deleted file.
    pub(crate) new_path: Option<RepoPath>,
    pub(crate) hunks: Vec<Hunk>,
}

impl FilePatch {
    /// The path the patch is about (new path, else old).
    pub(crate) fn path(&self) -> Option<&RepoPath> {
        self.new_path.as_ref().or(self.old_path.as_ref())
    }
}

fn header_path(rest: &str, line: usize) -> Result<Option<RepoPath>, PatchError> {
    let raw = rest.split('\t').next().unwrap_or(rest).trim();
    if raw == "/dev/null" {
        return Ok(None);
    }
    let stripped = raw
        .strip_prefix("a/")
        .or_else(|| raw.strip_prefix("b/"))
        .unwrap_or(raw);
    RepoPath::new(stripped)
        .map(Some)
        .map_err(|_| PatchError::Malformed {
            line,
            reason: "the file path is not a relative repository path",
        })
}

fn parse_range(text: &str, line: usize) -> Result<(u32, u32), PatchError> {
    let malformed = PatchError::Malformed {
        line,
        reason: "malformed hunk range",
    };
    let (start, len) = match text.split_once(',') {
        Some((s, l)) => (s, Some(l)),
        None => (text, None),
    };
    let parse = |s: &str| -> Result<u32, PatchError> {
        if s.is_empty() || s.len() > 9 || !s.bytes().all(|b| b.is_ascii_digit()) {
            return Err(malformed.clone());
        }
        s.parse().map_err(|_| malformed.clone())
    };
    Ok((parse(start)?, len.map_or(Ok(1), parse)?))
}

fn parse_hunk_header(text: &str, line: usize) -> Result<Hunk, PatchError> {
    let malformed = PatchError::Malformed {
        line,
        reason: "malformed hunk header",
    };
    let rest = text.strip_prefix("@@ ").ok_or_else(|| malformed.clone())?;
    let (ranges, _) = rest.split_once(" @@").ok_or_else(|| malformed.clone())?;
    let (old, new) = ranges.split_once(' ').ok_or_else(|| malformed.clone())?;
    let old = old.strip_prefix('-').ok_or_else(|| malformed.clone())?;
    let new = new.strip_prefix('+').ok_or(malformed)?;
    let (old_start, old_len) = parse_range(old, line)?;
    let (new_start, new_len) = parse_range(new, line)?;
    Ok(Hunk {
        old_start,
        old_len,
        new_start,
        new_len,
        lines: Vec::new(),
    })
}

/// Parses a unified diff (git or plain `diff -u` output).
pub(crate) fn parse(text: &str) -> Result<Vec<FilePatch>, PatchError> {
    let mut files: Vec<FilePatch> = Vec::new();
    let mut current: Option<FilePatch> = None;
    let mut old_header: Option<Option<RepoPath>> = None;
    let mut remaining: Option<(u32, u32)> = None;
    for (index, raw) in text.split('\n').enumerate() {
        let number = index.saturating_add(1);
        let line = raw.strip_suffix('\r').unwrap_or(raw);
        if let Some((old_left, new_left)) = remaining
            && (old_left > 0 || new_left > 0)
        {
            let hunk =
                current
                    .as_mut()
                    .and_then(|f| f.hunks.last_mut())
                    .ok_or(PatchError::Malformed {
                        line: number,
                        reason: "hunk line outside a hunk",
                    })?;
            let (kind, body) = match line.as_bytes().first() {
                Some(b' ') => (LineKind::Context, line.get(1..).unwrap_or_default()),
                None => (LineKind::Context, ""),
                Some(b'-') => (LineKind::Removed, line.get(1..).unwrap_or_default()),
                Some(b'+') => (LineKind::Added, line.get(1..).unwrap_or_default()),
                Some(b'\\') => continue,
                Some(_) => {
                    return Err(PatchError::Malformed {
                        line: number,
                        reason: "hunk is shorter than its header says",
                    });
                }
            };
            let (old_left, new_left) = match kind {
                LineKind::Context => (old_left.checked_sub(1), new_left.checked_sub(1)),
                LineKind::Removed => (old_left.checked_sub(1), Some(new_left)),
                LineKind::Added => (Some(old_left), new_left.checked_sub(1)),
            };
            let (Some(old_left), Some(new_left)) = (old_left, new_left) else {
                return Err(PatchError::Malformed {
                    line: number,
                    reason: "hunk is longer than its header says",
                });
            };
            hunk.lines.push((kind, body.to_owned()));
            remaining = Some((old_left, new_left));
            continue;
        }
        if line.starts_with("\\") {
            continue;
        }
        if let Some(rest) = line.strip_prefix("--- ") {
            if let Some(done) = current.take() {
                files.push(done);
            }
            old_header = Some(header_path(rest, number)?);
            continue;
        }
        if let Some(rest) = line.strip_prefix("+++ ") {
            let Some(old_path) = old_header.take() else {
                return Err(PatchError::Malformed {
                    line: number,
                    reason: "`+++` header without a `---` header",
                });
            };
            let new_path = header_path(rest, number)?;
            if old_path.is_none() && new_path.is_none() {
                return Err(PatchError::Malformed {
                    line: number,
                    reason: "both sides of a file header are /dev/null",
                });
            }
            if files.len() >= MAX_FILES {
                return Err(PatchError::TooLarge("more than 1000 files"));
            }
            current = Some(FilePatch {
                old_path,
                new_path,
                hunks: Vec::new(),
            });
            continue;
        }
        if line.starts_with("@@") {
            let Some(file) = current.as_mut() else {
                return Err(PatchError::Malformed {
                    line: number,
                    reason: "hunk before any file header",
                });
            };
            if file.hunks.len() >= MAX_HUNKS {
                return Err(PatchError::TooLarge("more than 10000 hunks in one file"));
            }
            let hunk = parse_hunk_header(line, number)?;
            remaining = Some((hunk.old_len, hunk.new_len));
            file.hunks.push(hunk);
            continue;
        }
        if current.as_ref().is_some_and(|f| !f.hunks.is_empty())
            && matches!(line.as_bytes().first(), Some(b' ' | b'+' | b'-'))
        {
            return Err(PatchError::Malformed {
                line: number,
                reason: "hunk is longer than its header says",
            });
        }
        // Anything else (git extended headers, commit text) is ignored.
    }
    if let Some((old_left, new_left)) = remaining
        && (old_left > 0 || new_left > 0)
    {
        return Err(PatchError::Malformed {
            line: text.split('\n').count(),
            reason: "the patch ends inside a hunk",
        });
    }
    if let Some(done) = current.take() {
        files.push(done);
    }
    if old_header.is_some() {
        return Err(PatchError::Malformed {
            line: text.split('\n').count(),
            reason: "`---` header without a `+++` header",
        });
    }
    if files.is_empty() {
        return Err(PatchError::Empty);
    }
    Ok(files)
}

/// Applies `patch` to `base`; `None` when a context or removed line does not
/// match the base (the patch was made against another version).
pub(crate) fn apply(base: &str, patch: &FilePatch) -> Option<String> {
    let lines: Vec<&str> = base.split('\n').collect();
    let mut out: Vec<String> = Vec::with_capacity(lines.len());
    let mut next = 0usize;
    for hunk in &patch.hunks {
        let start = usize::try_from(hunk.old_start.saturating_sub(1)).ok()?;
        let start = if hunk.old_len == 0 {
            usize::try_from(hunk.old_start).ok()?
        } else {
            start
        };
        if start < next || start > lines.len() {
            return None;
        }
        out.extend(lines.get(next..start)?.iter().map(|l| (*l).to_owned()));
        let mut at = start;
        for (kind, text) in &hunk.lines {
            match kind {
                LineKind::Context => {
                    if lines.get(at).map(|l| l.strip_suffix('\r').unwrap_or(l))
                        != Some(text.as_str())
                    {
                        return None;
                    }
                    out.push(text.clone());
                    at = at.saturating_add(1);
                }
                LineKind::Removed => {
                    if lines.get(at).map(|l| l.strip_suffix('\r').unwrap_or(l))
                        != Some(text.as_str())
                    {
                        return None;
                    }
                    at = at.saturating_add(1);
                }
                LineKind::Added => out.push(text.clone()),
            }
        }
        next = at;
    }
    out.extend(lines.get(next..)?.iter().map(|l| (*l).to_owned()));
    Some(out.join("\n"))
}

#[cfg(test)]
mod tests {
    use super::*;

    const BASE: &str = "a\nb\nc\nd\ne\n";

    fn patch(body: &str) -> String {
        format!("diff --git a/src/x.ts b/src/x.ts\n--- a/src/x.ts\n+++ b/src/x.ts\n{body}")
    }

    #[test]
    fn parses_and_applies_a_modification() {
        let text = patch("@@ -2,3 +2,3 @@ fn\n b\n-c\n+C\n d\n");
        let files = parse(&text).unwrap();
        assert_eq!(files.len(), 1);
        let file = &files[0];
        assert_eq!(file.path().unwrap().as_str(), "src/x.ts");
        assert_eq!(file.hunks[0].old_touched(), LineRange::new(3, 3).ok());
        assert_eq!(apply(BASE, file).unwrap(), "a\nb\nC\nd\ne\n");
    }

    #[test]
    fn insertions_and_new_files() {
        let text = patch("@@ -1,0 +2,1 @@\n+new\n");
        let files = parse(&text).unwrap();
        assert_eq!(apply(BASE, &files[0]).unwrap(), "a\nnew\nb\nc\nd\ne\n");
        let added = "--- /dev/null\n+++ b/src/new.ts\n@@ -0,0 +1,2 @@\n+x\n+y\n";
        let files = parse(added).unwrap();
        assert!(files[0].old_path.is_none());
        assert_eq!(apply("", &files[0]).unwrap(), "x\ny\n");
    }

    #[test]
    fn mismatched_context_does_not_apply() {
        let text = patch("@@ -2,2 +2,2 @@\n b\n-zzz\n+C\n");
        let files = parse(&text).unwrap();
        assert!(apply(BASE, &files[0]).is_none());
    }

    #[test]
    fn hostile_and_truncated_patches_are_rejected() {
        for text in [
            "",
            "just words\n",
            "--- a/../etc/passwd\n+++ b/../etc/passwd\n@@ -1 +1 @@\n-a\n+b\n",
            "--- /abs\n+++ /abs\n@@ -1 +1 @@\n-a\n+b\n",
            "--- a\\b\n+++ a\\b\n",
            "--- /dev/null\n+++ /dev/null\n",
            "+++ b/x\n",
            "--- a/x\n",
            "--- a/x\n+++ b/x\n@@ -1,3 +1,3 @@\n a\n",
            "--- a/x\n+++ b/x\n@@ -1,1 +1,1 @@\n a\n b\n c\n",
            "--- a/x\n+++ b/x\n@@ -x +1 @@\n",
            "--- a/x\n+++ b/x\n@@ -99999999999 +1 @@\n",
            "@@ -1 +1 @@\n-a\n+b\n",
        ] {
            assert!(parse(text).is_err(), "{text:?}");
        }
        let error = parse("--- a/../secret\n+++ b/x\n").unwrap_err();
        assert!(!error.to_string().contains("secret"));
    }
}
