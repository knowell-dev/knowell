//! Byte, line and string helpers shared by the parser, chunker and skeleton.
//!
//! All offsets are byte offsets into UTF-8 text. Every helper is total: out
//! of range offsets are clamped, never indexed.

use std::ops::Range;

use knowell_core::LineRange;

/// Byte offsets of the start of every line, for byte → line lookups.
///
/// Lines end at `\n`; a `\r` before it (CRLF) belongs to the line's content.
/// A trailing newline does not open a new (empty) last line.
#[derive(Debug, Clone)]
pub(crate) struct LineIndex {
    starts: Vec<usize>,
    len: usize,
}

impl LineIndex {
    pub(crate) fn new(text: &str) -> Self {
        let mut starts = vec![0];
        starts.extend(
            text.bytes()
                .enumerate()
                .filter(|&(_, b)| b == b'\n')
                .map(|(i, _)| i + 1),
        );
        if starts.len() > 1 && starts.last() == Some(&text.len()) {
            starts.pop();
        }
        Self {
            starts,
            len: text.len(),
        }
    }

    /// Number of lines (0 for empty text).
    pub(crate) fn line_count(&self) -> u32 {
        if self.len == 0 {
            0
        } else {
            to_u32(self.starts.len())
        }
    }

    /// 1-based line containing `byte` (clamped to the text).
    pub(crate) fn line_of(&self, byte: usize) -> u32 {
        let byte = byte.min(self.len);
        to_u32(self.starts.partition_point(|&start| start <= byte).max(1))
    }

    /// Lines covered by a byte range. An empty range maps to the line of its
    /// start; a range ending just after a newline does not include the next
    /// line.
    pub(crate) fn range(&self, bytes: &Range<usize>) -> Option<LineRange> {
        let start = self.line_of(bytes.start);
        let last = if bytes.end > bytes.start {
            bytes.end - 1
        } else {
            bytes.start
        };
        let end = self.line_of(last).max(start);
        LineRange::new(start, end).ok()
    }

    /// Byte offset where 1-based `line` starts.
    pub(crate) fn line_start(&self, line: u32) -> usize {
        let index = usize::try_from(line.saturating_sub(1)).unwrap_or(usize::MAX);
        self.starts.get(index).copied().unwrap_or(self.len)
    }
}

pub(crate) fn to_u32(value: usize) -> u32 {
    u32::try_from(value).unwrap_or(u32::MAX)
}

/// The text of a byte range, or `""` if the range is out of bounds or not on
/// character boundaries.
pub(crate) fn slice<'a>(text: &'a str, range: &Range<usize>) -> &'a str {
    text.get(range.clone()).unwrap_or("")
}

/// The largest character boundary `<= index`.
pub(crate) fn floor_boundary(text: &str, index: usize) -> usize {
    if index >= text.len() {
        return text.len();
    }
    let mut i = index;
    while i > 0 && !text.is_char_boundary(i) {
        i -= 1;
    }
    i
}

/// At most `max` bytes of `text`, cut on a character boundary.
pub(crate) fn prefix(text: &str, max: usize) -> &str {
    let end = floor_boundary(text, max);
    text.get(..end).unwrap_or("")
}

/// Bounds `text` to `max` bytes, appending `…` when something was cut.
pub(crate) fn bounded(text: &str, max: usize) -> String {
    if text.len() <= max {
        text.to_owned()
    } else {
        let mut out = prefix(text, max).trim_end().to_owned();
        out.push('…');
        out
    }
}

/// Moves `end` back over trailing whitespace, never before `start`.
pub(crate) fn trim_end_offset(text: &str, start: usize, end: usize) -> usize {
    let segment = text.get(start..end).unwrap_or("");
    start + segment.trim_end().len()
}

/// Byte offset of the start of the line containing `byte`.
pub(crate) fn line_start_of(text: &str, byte: usize) -> usize {
    let byte = floor_boundary(text, byte);
    text.get(..byte)
        .and_then(|before| before.rfind('\n'))
        .map_or(0, |i| i + 1)
}

/// Whether only whitespace precedes `byte` on its line.
pub(crate) fn starts_line(text: &str, byte: usize) -> bool {
    let start = line_start_of(text, byte);
    slice(text, &(start..byte)).trim().is_empty()
}

/// The whitespace indentation of the line containing `byte`, if only
/// whitespace precedes `byte` on that line.
pub(crate) fn indent_before(text: &str, byte: usize) -> &str {
    let start = line_start_of(text, byte);
    let before = slice(text, &(start..byte));
    if before.trim().is_empty() { before } else { "" }
}

/// Collapses all whitespace runs to single spaces.
pub(crate) fn one_line(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Removes one pair of matching quotes (`"…"`, `'…'`, `` `…` ``, `<…>`).
pub(crate) fn strip_quotes(text: &str) -> &str {
    let text = text.trim();
    for (open, close) in [('"', '"'), ('\'', '\''), ('`', '`'), ('<', '>')] {
        if let Some(inner) = text
            .strip_prefix(open)
            .and_then(|rest| rest.strip_suffix(close))
        {
            return inner;
        }
    }
    text
}

/// The first paragraph of a doc comment: at most `max_lines` trimmed lines,
/// or, for `max_lines == 1`, the whole paragraph on one line (bounded).
pub(crate) fn summary(doc: Option<&str>, max_lines: usize) -> Option<String> {
    let paragraph = doc?.split("\n\n").next()?;
    if max_lines <= 1 {
        let line = one_line(paragraph);
        return (!line.is_empty()).then(|| bounded(&line, 300));
    }
    let lines: Vec<&str> = paragraph
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .take(max_lines)
        .collect();
    (!lines.is_empty()).then(|| lines.join("\n"))
}

/// Whether `name` looks like a constant (`MAX_RETRIES`, `PI`).
pub(crate) fn is_upper_case(name: &str) -> bool {
    name.chars().any(|c| c.is_alphabetic())
        && name
            .chars()
            .all(|c| c.is_uppercase() || c.is_ascii_digit() || c == '_')
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn line_index_handles_trailing_newlines_and_crlf() {
        let index = LineIndex::new("a\r\nb\r\n");
        assert_eq!(index.line_count(), 2);
        assert_eq!(index.line_of(0), 1);
        assert_eq!(index.line_of(3), 2);
        assert_eq!(index.range(&(0..6)).unwrap(), LineRange::new(1, 2).unwrap());
        assert_eq!(index.range(&(0..3)).unwrap(), LineRange::new(1, 1).unwrap());
        assert_eq!(LineIndex::new("").line_count(), 0);
        assert_eq!(LineIndex::new("x").line_count(), 1);
        assert_eq!(LineIndex::new("x\n\n").line_count(), 2);
        assert_eq!(index.line_start(2), 3);
        assert_eq!(index.line_start(99), 6);
    }

    #[test]
    fn helpers_are_total() {
        assert_eq!(slice("abc", &(2..9)), "");
        assert_eq!(prefix("héllo", 2), "h");
        assert_eq!(bounded("abcdef", 3), "abc…");
        assert_eq!(strip_quotes("\"x\""), "x");
        assert_eq!(strip_quotes("<stdio.h>"), "stdio.h");
        assert_eq!(strip_quotes("'"), "'");
        assert!(starts_line("  x", 2));
        assert!(!starts_line("a x", 2));
        assert_eq!(indent_before("a\n    b", 6), "    ");
        assert!(is_upper_case("MAX_2"));
        assert!(!is_upper_case("Max"));
        assert!(!is_upper_case("_"));
        assert_eq!(trim_end_offset("ab  \n", 0, 5), 2);
    }
}
