//! Validated identifiers, timestamps and the normalised subject key.

use std::fmt;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use time::OffsetDateTime;
use uuid::Uuid;

use crate::error::KnowledgeError;

macro_rules! uuid_id {
    ($(#[$meta:meta])* $name:ident) => {
        $(#[$meta])*
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize, JsonSchema)]
        #[serde(transparent)]
        pub struct $name(#[schemars(with = "String")] Uuid);

        impl $name {
            /// Wraps an existing UUID.
            pub fn from_uuid(id: Uuid) -> Self {
                Self(id)
            }

            /// Generates a fresh time-ordered (v7) identifier. This reads the
            /// clock and the system RNG; domain logic itself never calls it.
            pub fn generate() -> Self {
                Self(Uuid::now_v7())
            }

            /// The underlying UUID.
            pub fn as_uuid(&self) -> Uuid {
                self.0
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                write!(f, "{}", self.0.hyphenated())
            }
        }
    };
}

uuid_id!(
    /// Identifier of a [`KnowledgeRecord`](crate::KnowledgeRecord).
    RecordId
);
uuid_id!(
    /// Identifier of a [`Task`](crate::Task).
    TaskId
);

fn check_label(s: &str) -> Result<(), &'static str> {
    if s.is_empty() {
        Err("must not be empty")
    } else if s.len() > 256 {
        Err("is longer than 256 bytes")
    } else if s.trim() != s {
        Err("must not start or end with whitespace")
    } else if s.chars().any(char::is_control) {
        Err("must not contain control characters")
    } else {
        Ok(())
    }
}

fn check_commit(s: &str) -> Result<(), &'static str> {
    if !(7..=64).contains(&s.len()) {
        Err("must be 7 to 64 hexadecimal digits")
    } else if !s
        .bytes()
        .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    {
        Err("must be lowercase hexadecimal")
    } else {
        Ok(())
    }
}

macro_rules! text_id {
    ($(#[$meta:meta])* $name:ident, $label:literal, $check:path) => {
        $(#[$meta])*
        #[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize, JsonSchema)]
        #[serde(try_from = "String", into = "String")]
        #[schemars(with = "String")]
        pub struct $name(String);

        impl $name {
            /// Validates and wraps `value`.
            pub fn new(value: impl Into<String>) -> Result<Self, KnowledgeError> {
                let value = value.into();
                $check(&value).map_err(|reason| KnowledgeError::InvalidId { kind: $label, reason })?;
                Ok(Self(value))
            }

            /// The identifier as text.
            pub fn as_str(&self) -> &str {
                &self.0
            }
        }

        impl TryFrom<String> for $name {
            type Error = KnowledgeError;
            fn try_from(value: String) -> Result<Self, Self::Error> {
                Self::new(value)
            }
        }

        impl From<$name> for String {
            fn from(id: $name) -> String {
                id.0
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str(&self.0)
            }
        }
    };
}

text_id!(
    /// Identifier of a human user, as issued by the identity provider.
    UserId, "user id", check_label
);
text_id!(
    /// Identifier of one agent session.
    SessionId, "session id", check_label
);
text_id!(
    /// Name of the agent client (for example `codex`).
    ClientId, "client id", check_label
);
text_id!(
    /// Stable identifier of a code symbol, as produced by the code index.
    SymbolId, "symbol id", check_label
);
text_id!(
    /// Identifier of a source view of a project (a ref, worktree or snapshot).
    ViewId, "view id", check_label
);
text_id!(
    /// A commit id: 7 to 64 lowercase hexadecimal digits.
    CommitId, "commit id", check_commit
);

/// A point in time as whole seconds since the Unix epoch (UTC).
///
/// Domain functions never read the clock; callers pass the time in, which
/// keeps every rule deterministic and testable.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize, JsonSchema,
)]
#[serde(transparent)]
pub struct Timestamp(i64);

impl Timestamp {
    /// Wraps seconds since the Unix epoch.
    pub fn from_unix_seconds(seconds: i64) -> Self {
        Self(seconds)
    }

    /// Seconds since the Unix epoch.
    pub fn unix_seconds(&self) -> i64 {
        self.0
    }

    /// Converts from a `time` value, truncating sub-second precision.
    pub fn from_datetime(value: OffsetDateTime) -> Self {
        Self(value.unix_timestamp())
    }

    /// Converts to a UTC `time` value; `None` if out of the representable range.
    pub fn to_datetime(&self) -> Option<OffsetDateTime> {
        OffsetDateTime::from_unix_timestamp(self.0).ok()
    }
}

impl fmt::Display for Timestamp {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// Normalised key of what a record is about, for example `payments.idempotency`.
///
/// Two records conflict only if they share a subject, so the key must be
/// canonical: lowercase ASCII, segments separated by `.`. Whitespace, `/` and
/// `:` in the input become `.`; repeated separators collapse.
#[derive(
    Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize, JsonSchema,
)]
#[serde(try_from = "String", into = "String")]
#[schemars(with = "String")]
pub struct Subject(String);

impl Subject {
    /// Maximum length in bytes.
    pub const MAX_LEN: usize = 128;

    /// Normalises and validates `raw`.
    pub fn new(raw: &str) -> Result<Self, KnowledgeError> {
        let mut out = String::with_capacity(raw.len());
        for c in raw.trim().chars() {
            let c = c.to_ascii_lowercase();
            let mapped = if c.is_whitespace() || c == '/' || c == ':' {
                '.'
            } else {
                c
            };
            match mapped {
                '.' => {
                    if !out.is_empty() && !out.ends_with('.') {
                        out.push('.');
                    }
                }
                'a'..='z' | '0'..='9' | '_' | '-' => out.push(mapped),
                _ => {
                    return Err(KnowledgeError::InvalidSubject(
                        "only ASCII letters, digits, `_`, `-` and separators are allowed",
                    ));
                }
            }
        }
        while out.ends_with('.') {
            out.pop();
        }
        if out.is_empty() {
            return Err(KnowledgeError::InvalidSubject("must not be empty"));
        }
        if out.len() > Self::MAX_LEN {
            return Err(KnowledgeError::InvalidSubject("is longer than 128 bytes"));
        }
        Ok(Self(out))
    }

    /// The normalised key.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl TryFrom<String> for Subject {
    type Error = KnowledgeError;
    fn try_from(value: String) -> Result<Self, Self::Error> {
        Self::new(&value)
    }
}

impl From<Subject> for String {
    fn from(s: Subject) -> String {
        s.0
    }
}

impl fmt::Display for Subject {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn subject_normalises() {
        assert_eq!(
            Subject::new("  Payments / Idempotency ").unwrap().as_str(),
            "payments.idempotency"
        );
        assert_eq!(Subject::new("a::b..c").unwrap().as_str(), "a.b.c");
        assert_eq!(Subject::new("a.b.").unwrap().as_str(), "a.b");
    }

    #[test]
    fn subject_rejects_bad_input() {
        let long = "x".repeat(129);
        for bad in ["", "   ", "...", "ödeme", "a$b", long.as_str()] {
            assert!(Subject::new(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn text_ids_validate() {
        assert!(UserId::new("u-1").is_ok());
        assert!(UserId::new("").is_err());
        assert!(UserId::new(" x").is_err());
        assert!(UserId::new("a\nb").is_err());
        assert!(CommitId::new("abc1234").is_ok());
        assert!(CommitId::new("ABC1234").is_err());
        assert!(CommitId::new("abc").is_err());
        assert!(serde_json::from_str::<CommitId>("\"zzzzzzz\"").is_err());
    }

    #[test]
    fn timestamp_round_trips_datetime() {
        let t = Timestamp::from_unix_seconds(1_700_000_000);
        assert_eq!(Timestamp::from_datetime(t.to_datetime().unwrap()), t);
        assert_eq!(serde_json::to_string(&t).unwrap(), "1700000000");
    }

    #[test]
    fn uuid_ids_display_and_serde() {
        let id = RecordId::from_uuid(Uuid::from_u128(7));
        assert_eq!(id.to_string(), "00000000-0000-0000-0000-000000000007");
        let json = serde_json::to_string(&id).unwrap();
        assert_eq!(serde_json::from_str::<RecordId>(&json).unwrap(), id);
        assert_ne!(RecordId::generate(), RecordId::generate());
    }
}
