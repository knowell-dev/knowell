//! Identifiers and scalar values that appear in tool inputs and outputs.
//!
//! Opaque ids ([`ContextId`], [`ResultId`], [`JobId`], [`MemoryId`],
//! [`TaskId`], [`CheckpointId`]) are minted by the engine and handed back by
//! agents verbatim. They are validated only for shape (printable ASCII, no
//! whitespace, bounded length) so a hostile or truncated value is rejected
//! before it reaches the engine. Validation errors never echo the value.

use std::borrow::Cow;
use std::fmt;
use std::str::FromStr;

use schemars::{JsonSchema, Schema, SchemaGenerator};
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use serde_json::{Map, Value};

/// Error returned when text is not a valid identifier or scalar value.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum IdError {
    /// The value is empty.
    #[error("{kind} must not be empty")]
    Empty {
        /// Which kind of value was rejected.
        kind: &'static str,
    },
    /// The value is longer than allowed.
    #[error("{kind} is longer than {max} bytes")]
    TooLong {
        /// Which kind of value was rejected.
        kind: &'static str,
        /// Maximum length in bytes.
        max: usize,
    },
    /// The value contains a character outside the allowed set or has the
    /// wrong shape.
    #[error("{kind} is malformed: {expected}")]
    Malformed {
        /// Which kind of value was rejected.
        kind: &'static str,
        /// What a valid value looks like.
        expected: &'static str,
    },
}

/// Builds a string schema with optional length bounds, pattern and examples.
fn string_schema(
    description: &str,
    min_len: Option<usize>,
    max_len: Option<usize>,
    pattern: Option<&str>,
    examples: &[&str],
) -> Schema {
    let mut map = Map::new();
    map.insert("type".into(), Value::from("string"));
    map.insert("description".into(), Value::from(description));
    if let Some(min) = min_len {
        map.insert("minLength".into(), Value::from(min));
    }
    if let Some(max) = max_len {
        map.insert("maxLength".into(), Value::from(max));
    }
    if let Some(pattern) = pattern {
        map.insert("pattern".into(), Value::from(pattern));
    }
    if !examples.is_empty() {
        map.insert(
            "examples".into(),
            Value::Array(examples.iter().map(|e| Value::from(*e)).collect()),
        );
    }
    Schema::from(map)
}

/// Checks the shared opaque-id shape: 1..=`max` bytes of printable,
/// non-space ASCII (`!` through `~`).
fn check_opaque(kind: &'static str, value: &str, max: usize) -> Result<(), IdError> {
    if value.is_empty() {
        return Err(IdError::Empty { kind });
    }
    if value.len() > max {
        return Err(IdError::TooLong { kind, max });
    }
    if !value.bytes().all(|b| b.is_ascii_graphic()) {
        return Err(IdError::Malformed {
            kind,
            expected: "printable ASCII without spaces",
        });
    }
    Ok(())
}

macro_rules! opaque_id {
    (
        $(#[$doc:meta])*
        $name:ident, kind = $kind:literal, max = $max:literal, example = $example:literal
    ) => {
        $(#[$doc])*
        #[derive(Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
        pub struct $name(String);

        impl $name {
            /// Maximum length in bytes.
            pub const MAX_LEN: usize = $max;

            /// Validates and wraps `value`: 1 to [`Self::MAX_LEN`] bytes of
            /// printable ASCII without spaces.
            pub fn new(value: impl Into<String>) -> Result<Self, IdError> {
                let value = value.into();
                check_opaque($kind, &value, $max)?;
                Ok(Self(value))
            }

            /// The id as text.
            pub fn as_str(&self) -> &str {
                &self.0
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str(&self.0)
            }
        }

        impl fmt::Debug for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                write!(f, "{}({})", stringify!($name), self.0)
            }
        }

        impl FromStr for $name {
            type Err = IdError;

            fn from_str(s: &str) -> Result<Self, Self::Err> {
                Self::new(s)
            }
        }

        impl AsRef<str> for $name {
            fn as_ref(&self) -> &str {
                &self.0
            }
        }

        impl Serialize for $name {
            fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
                serializer.serialize_str(&self.0)
            }
        }

        impl<'de> Deserialize<'de> for $name {
            fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
                let s = String::deserialize(deserializer)?;
                Self::new(s).map_err(serde::de::Error::custom)
            }
        }

        impl JsonSchema for $name {
            fn schema_name() -> Cow<'static, str> {
                stringify!($name).into()
            }

            fn json_schema(_: &mut SchemaGenerator) -> Schema {
                string_schema(
                    concat!("Opaque ", $kind, "; pass it back exactly as received."),
                    Some(1),
                    Some($max),
                    Some("^[!-~]+$"),
                    &[$example],
                )
            }
        }
    };
}

opaque_id!(
    /// Handle returned by `open_workspace`. It pins the workspace and the
    /// view (ref + commit) of every project, so concurrent agents never
    /// change each other's selection.
    ContextId, kind = "context id", max = 128, example = "ctx-7f3a9c"
);

opaque_id!(
    /// Stable id of a result item (code chunk, symbol, contract, context
    /// entry). It identifies an exact version and can be passed to `fetch`.
    ResultId, kind = "result id", max = 512, example = "kn:billing-api:3f9a1c2b7d10:src/payments/payment.service.ts#L12-L40"
);

opaque_id!(
    /// Id of a long-running operation; pass it back to the same tool or to
    /// `index_status` to poll.
    JobId, kind = "job id", max = 128, example = "job-42"
);

opaque_id!(
    /// Id of a memory record (decision, rule, note, finding, …).
    MemoryId, kind = "memory id", max = 128, example = "mem-17"
);

opaque_id!(
    /// Id of a task (goal, progress, decisions, open questions).
    TaskId, kind = "task id", max = 128, example = "task-3"
);

opaque_id!(
    /// Id of one saved checkpoint of a task.
    CheckpointId, kind = "checkpoint id", max = 128, example = "cp-3-2"
);

/// A full git commit id: 40 (SHA-1) or 64 (SHA-256) lowercase hex digits.
#[derive(Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct CommitId(String);

impl CommitId {
    /// Validates and wraps a full lowercase hex commit id.
    pub fn new(value: impl Into<String>) -> Result<Self, IdError> {
        let value = value.into();
        if value.is_empty() {
            return Err(IdError::Empty { kind: "commit id" });
        }
        let full_length = value.len() == 40 || value.len() == 64;
        let hex = value
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b));
        if !(full_length && hex) {
            return Err(IdError::Malformed {
                kind: "commit id",
                expected: "40 or 64 lowercase hexadecimal digits",
            });
        }
        Ok(Self(value))
    }

    /// The id as text.
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// The first 12 hex digits, for compact renderings.
    pub fn short(&self) -> &str {
        self.0.get(..12).unwrap_or(&self.0)
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

impl FromStr for CommitId {
    type Err = IdError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::new(s)
    }
}

impl Serialize for CommitId {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.0)
    }
}

impl<'de> Deserialize<'de> for CommitId {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let s = String::deserialize(deserializer)?;
        Self::new(s).map_err(serde::de::Error::custom)
    }
}

impl JsonSchema for CommitId {
    fn schema_name() -> Cow<'static, str> {
        "CommitId".into()
    }

    fn json_schema(_: &mut SchemaGenerator) -> Schema {
        string_schema(
            "Full git commit id (40 or 64 lowercase hex digits).",
            None,
            None,
            Some("^([0-9a-f]{40}|[0-9a-f]{64})$"),
            &[],
        )
    }
}

/// A UTC timestamp in RFC 3339 form: `YYYY-MM-DDTHH:MM:SS[.fraction]Z`.
///
/// Only the UTC (`Z`) form is accepted so that timestamps sort and compare
/// as text.
#[derive(Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Timestamp(String);

impl Timestamp {
    /// Validates and wraps an RFC 3339 UTC timestamp.
    pub fn new(value: impl Into<String>) -> Result<Self, IdError> {
        let value = value.into();
        if value.is_empty() {
            return Err(IdError::Empty { kind: "timestamp" });
        }
        if value.len() > 40 || !timestamp_shape_ok(value.as_bytes()) {
            return Err(IdError::Malformed {
                kind: "timestamp",
                expected: "RFC 3339 UTC such as 2026-10-02T09:30:00Z",
            });
        }
        Ok(Self(value))
    }

    /// The timestamp as text.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// Checks `YYYY-MM-DDTHH:MM:SS[.f{1,9}]Z` and plausible field ranges.
fn timestamp_shape_ok(b: &[u8]) -> bool {
    let digits = |range: std::ops::Range<usize>| -> Option<u32> {
        let slice = b.get(range)?;
        if slice.is_empty() || !slice.iter().all(u8::is_ascii_digit) {
            return None;
        }
        slice.iter().try_fold(0u32, |acc, d| {
            acc.checked_mul(10)?.checked_add(u32::from(d - b'0'))
        })
    };
    let sep = |i: usize, c: u8| b.get(i) == Some(&c);
    let (Some(_year), Some(month), Some(day), Some(hour), Some(minute), Some(second)) = (
        digits(0..4),
        digits(5..7),
        digits(8..10),
        digits(11..13),
        digits(14..16),
        digits(17..19),
    ) else {
        return false;
    };
    if !(sep(4, b'-') && sep(7, b'-') && sep(10, b'T') && sep(13, b':') && sep(16, b':')) {
        return false;
    }
    if !((1..=12).contains(&month)
        && (1..=31).contains(&day)
        && hour <= 23
        && minute <= 59
        && second <= 60)
    {
        return false;
    }
    match b.get(19..) {
        Some([b'Z']) => true,
        Some([b'.', rest @ ..]) => match rest.split_last() {
            Some((b'Z', fraction)) => {
                (1..=9).contains(&fraction.len()) && fraction.iter().all(u8::is_ascii_digit)
            }
            _ => false,
        },
        _ => false,
    }
}

impl fmt::Display for Timestamp {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl fmt::Debug for Timestamp {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Timestamp({})", self.0)
    }
}

impl FromStr for Timestamp {
    type Err = IdError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::new(s)
    }
}

impl Serialize for Timestamp {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.0)
    }
}

impl<'de> Deserialize<'de> for Timestamp {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let s = String::deserialize(deserializer)?;
        Self::new(s).map_err(serde::de::Error::custom)
    }
}

impl JsonSchema for Timestamp {
    fn schema_name() -> Cow<'static, str> {
        "Timestamp".into()
    }

    fn json_schema(_: &mut SchemaGenerator) -> Schema {
        string_schema(
            "UTC timestamp, RFC 3339 (YYYY-MM-DDTHH:MM:SS[.fraction]Z).",
            None,
            None,
            Some(r"^\d{4}-\d{2}-\d{2}T\d{2}:\d{2}:\d{2}(\.\d{1,9})?Z$"),
            &["2026-10-02T09:30:00Z"],
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn opaque_ids_accept_printable_ascii() {
        for ok in ["ctx-1", "kn:a:b/c#L1-L2", "~!@#$%^&*()", "x"] {
            assert!(ResultId::new(ok).is_ok(), "{ok}");
        }
        assert!(ContextId::new("c".repeat(ContextId::MAX_LEN)).is_ok());
    }

    #[test]
    fn opaque_ids_reject_hostile_input_without_echo() {
        assert_eq!(
            ContextId::new(""),
            Err(IdError::Empty { kind: "context id" })
        );
        let long = ContextId::new("c".repeat(ContextId::MAX_LEN + 1)).unwrap_err();
        assert!(matches!(long, IdError::TooLong { max: 128, .. }));
        for bad in [
            "a b",
            "tab\there",
            "nl\n",
            "nul\0",
            "ünïcode",
            "\u{202e}rtl",
            "a\u{7f}",
        ] {
            let err = JobId::new(bad).unwrap_err();
            assert!(matches!(err, IdError::Malformed { .. }), "{bad:?}");
            assert!(!err.to_string().contains(bad), "error echoes input");
        }
    }

    #[test]
    fn opaque_ids_validate_through_serde() {
        assert!(serde_json::from_str::<MemoryId>("\"mem-1\"").is_ok());
        assert!(serde_json::from_str::<MemoryId>("\"mem 1\"").is_err());
        assert!(serde_json::from_str::<MemoryId>("17").is_err());
        let id = TaskId::new("task-3").unwrap();
        assert_eq!(serde_json::to_string(&id).unwrap(), "\"task-3\"");
    }

    #[test]
    fn commit_ids() {
        let sha1 = "0123456789abcdef0123456789abcdef01234567";
        let c = CommitId::new(sha1).unwrap();
        assert_eq!(c.short(), "0123456789ab");
        assert!(CommitId::new("ab".repeat(32)).is_ok());
        for bad in [
            "",
            "abc",
            &sha1.to_uppercase(),
            &format!("{sha1}0"),
            &"g".repeat(40),
        ] {
            assert!(CommitId::new(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn timestamps() {
        for ok in [
            "2026-10-02T09:30:00Z",
            "2026-10-02T09:30:00.5Z",
            "2026-10-02T09:30:00.123456789Z",
            "2026-12-31T23:59:60Z",
        ] {
            assert!(Timestamp::new(ok).is_ok(), "{ok}");
        }
        for bad in [
            "",
            "2026-10-02",
            "2026-10-02T09:30:00",
            "2026-10-02T09:30:00+02:00",
            "2026-13-02T09:30:00Z",
            "2026-10-00T09:30:00Z",
            "2026-10-02T24:30:00Z",
            "2026-10-02T09:30:00.Z",
            "2026-10-02T09:30:00.1234567890Z",
            "2026-10-02 09:30:00Z",
            "２026-10-02T09:30:00Z",
            "2026-10-02T09:30:00ZZ",
        ] {
            assert!(Timestamp::new(bad).is_err(), "{bad}");
        }
    }
}
