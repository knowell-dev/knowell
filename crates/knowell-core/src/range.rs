use std::fmt;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// A 1-based, inclusive span of lines in a file version.
///
/// A line range alone never identifies code: it is always paired with a
/// view, a path and a content hash, because lines shift between versions.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize, JsonSchema)]
#[serde(try_from = "RawLineRange", into = "RawLineRange")]
pub struct LineRange {
    start: u32,
    end: u32,
}

/// Error returned for an invalid [`LineRange`].
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum LineRangeError {
    /// Line numbers start at 1.
    #[error("line numbers start at 1")]
    Zero,
    /// `end` is before `start`.
    #[error("line range end {end} is before start {start}")]
    Reversed {
        /// First line.
        start: u32,
        /// Last line.
        end: u32,
    },
}

impl LineRange {
    /// Creates a range covering lines `start..=end`.
    pub fn new(start: u32, end: u32) -> Result<Self, LineRangeError> {
        if start == 0 || end == 0 {
            return Err(LineRangeError::Zero);
        }
        if end < start {
            return Err(LineRangeError::Reversed { start, end });
        }
        Ok(Self { start, end })
    }

    /// First line (1-based).
    pub fn start(&self) -> u32 {
        self.start
    }

    /// Last line (1-based, inclusive).
    pub fn end(&self) -> u32 {
        self.end
    }

    /// Number of lines covered (at least 1).
    pub fn line_count(&self) -> u32 {
        self.end - self.start + 1
    }

    /// Whether the two ranges share at least one line.
    pub fn overlaps(&self, other: &LineRange) -> bool {
        self.start <= other.end && other.start <= self.end
    }
}

impl fmt::Display for LineRange {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.start == self.end {
            write!(f, "L{}", self.start)
        } else {
            write!(f, "L{}-L{}", self.start, self.end)
        }
    }
}

impl fmt::Debug for LineRange {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(self, f)
    }
}

/// Wire form of [`LineRange`]; validated on the way in.
#[derive(Serialize, Deserialize, JsonSchema)]
struct RawLineRange {
    /// First line (1-based).
    start: u32,
    /// Last line (1-based, inclusive).
    end: u32,
}

impl TryFrom<RawLineRange> for LineRange {
    type Error = LineRangeError;

    fn try_from(raw: RawLineRange) -> Result<Self, Self::Error> {
        Self::new(raw.start, raw.end)
    }
}

impl From<LineRange> for RawLineRange {
    fn from(range: LineRange) -> Self {
        Self {
            start: range.start,
            end: range.end,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validates() {
        assert_eq!(LineRange::new(0, 3), Err(LineRangeError::Zero));
        assert!(matches!(
            LineRange::new(5, 3),
            Err(LineRangeError::Reversed { .. })
        ));
        let r = LineRange::new(3, 7).unwrap();
        assert_eq!(r.line_count(), 5);
        assert_eq!(r.to_string(), "L3-L7");
        assert_eq!(LineRange::new(4, 4).unwrap().to_string(), "L4");
    }

    #[test]
    fn overlap() {
        let a = LineRange::new(1, 5).unwrap();
        assert!(a.overlaps(&LineRange::new(5, 9).unwrap()));
        assert!(!a.overlaps(&LineRange::new(6, 9).unwrap()));
    }

    #[test]
    fn serde_validates() {
        let r: LineRange = serde_json::from_str(r#"{"start":2,"end":4}"#).unwrap();
        assert_eq!(r, LineRange::new(2, 4).unwrap());
        assert!(serde_json::from_str::<LineRange>(r#"{"start":4,"end":2}"#).is_err());
        assert_eq!(serde_json::to_string(&r).unwrap(), r#"{"start":2,"end":4}"#);
    }
}
