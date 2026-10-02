use std::fmt;
use std::str::FromStr;

use schemars::{JsonSchema, Schema, SchemaGenerator};
use serde::{Deserialize, Deserializer, Serialize, Serializer};

/// Identifier of a workspace, project or profile: a lowercase slug of
/// 1–64 characters from `[a-z0-9_-]`, starting with a letter or digit.
///
/// Names appear in URLs, MCP arguments, file names and log lines, so they
/// are restricted to characters that need no escaping anywhere.
#[derive(Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Name(String);

/// Error returned when text is not a valid [`Name`].
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum NameError {
    /// The text is empty.
    #[error("name must not be empty")]
    Empty,
    /// The text is longer than [`Name::MAX_LEN`].
    #[error("name `{0}` is longer than 64 characters")]
    TooLong(String),
    /// The text contains a character outside `[a-z0-9_-]` or starts with `_`/`-`.
    #[error(
        "name `{0}` must use lowercase letters, digits, `-` or `_` and start with a letter or digit"
    )]
    InvalidCharacter(String),
}

impl Name {
    /// Maximum length in bytes (all allowed characters are ASCII).
    pub const MAX_LEN: usize = 64;

    /// Validates and wraps `value`.
    pub fn new(value: impl Into<String>) -> Result<Self, NameError> {
        let value = value.into();
        let mut chars = value.chars();
        let Some(first) = chars.next() else {
            return Err(NameError::Empty);
        };
        if value.len() > Self::MAX_LEN {
            return Err(NameError::TooLong(value));
        }
        let first_ok = first.is_ascii_lowercase() || first.is_ascii_digit();
        let rest_ok =
            chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-' || c == '_');
        if !(first_ok && rest_ok) {
            return Err(NameError::InvalidCharacter(value));
        }
        Ok(Self(value))
    }

    /// The name as text.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for Name {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl fmt::Debug for Name {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Name({})", self.0)
    }
}

impl FromStr for Name {
    type Err = NameError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::new(s)
    }
}

impl AsRef<str> for Name {
    fn as_ref(&self) -> &str {
        &self.0
    }
}

impl Serialize for Name {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.0)
    }
}

impl<'de> Deserialize<'de> for Name {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let s = String::deserialize(deserializer)?;
        Self::new(s).map_err(serde::de::Error::custom)
    }
}

impl JsonSchema for Name {
    fn schema_name() -> std::borrow::Cow<'static, str> {
        "Name".into()
    }

    fn json_schema(_: &mut SchemaGenerator) -> Schema {
        crate::schema::string_schema(
            "Lowercase slug (a-z, 0-9, '-', '_'; max 64).",
            Some("^[a-z0-9][a-z0-9_-]{0,63}$"),
            &[],
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_slugs() {
        let long = "x".repeat(64);
        for ok in ["a", "billing-api", "web_2", "0day", long.as_str()] {
            assert!(Name::new(ok).is_ok(), "{ok}");
        }
    }

    #[test]
    fn rejects_non_slugs() {
        assert_eq!(Name::new(""), Err(NameError::Empty));
        assert!(matches!(
            Name::new("x".repeat(65)),
            Err(NameError::TooLong(_))
        ));
        for bad in ["Billing", "-a", "_a", "a b", "a/b", "ödeme", "a.b"] {
            assert!(
                matches!(Name::new(bad), Err(NameError::InvalidCharacter(_))),
                "{bad}"
            );
        }
    }

    #[test]
    fn serde_validates() {
        assert!(serde_json::from_str::<Name>("\"api\"").is_ok());
        assert!(serde_json::from_str::<Name>("\"API\"").is_err());
    }
}
