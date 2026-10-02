use std::fmt;
use std::path::PathBuf;
use std::str::FromStr;

use schemars::{JsonSchema, Schema, SchemaGenerator};
use serde::{Deserialize, Deserializer, Serialize, Serializer};

/// A reference to a secret held outside the configuration.
///
/// Configuration files store *where* a secret lives, never the secret
/// itself. Resolution happens in `knowell-secrets` at the moment of use.
///
/// | Text | Meaning |
/// |---|---|
/// | `env:GEMINI_API_KEY` | value of an environment variable of the running process |
/// | `file:/run/secrets/gemini` | contents of a file (trailing newline trimmed) |
///
/// Parsing errors deliberately never echo the rejected text: when a user
/// pastes a real API key where a reference belongs, the key must not end
/// up in an error message, a log line or a terminal scrollback.
#[derive(Clone, PartialEq, Eq, Hash)]
pub enum SecretRef {
    /// An environment variable name (`[A-Za-z_][A-Za-z0-9_]*`).
    Env(String),
    /// A file path whose contents are the secret.
    File(PathBuf),
}

/// Error returned when text is not a valid [`SecretRef`]. Never contains
/// the rejected input.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum SecretRefError {
    /// The text has no `env:` / `file:` prefix — possibly a literal secret.
    #[error(
        "expected a secret reference such as `env:NAME` or `file:/path`; secret values must not be written into configuration (the value was not echoed)"
    )]
    NotAReference,
    /// `env:` is followed by an invalid variable name.
    #[error(
        "`env:` must be followed by an environment variable name made of letters, digits and `_` (the value was not echoed)"
    )]
    InvalidEnvName,
    /// `file:` is followed by nothing.
    #[error("`file:` must be followed by a path")]
    EmptyFilePath,
}

impl SecretRef {
    /// Short description safe for logs and UI, e.g. `env:GEMINI_API_KEY`.
    pub fn describe(&self) -> String {
        self.to_string()
    }
}

impl fmt::Display for SecretRef {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SecretRef::Env(name) => write!(f, "env:{name}"),
            SecretRef::File(path) => write!(f, "file:{}", path.display()),
        }
    }
}

impl fmt::Debug for SecretRef {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "SecretRef({self})")
    }
}

impl FromStr for SecretRef {
    type Err = SecretRefError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        if let Some(name) = s.strip_prefix("env:") {
            let mut chars = name.chars();
            let first_ok = chars
                .next()
                .is_some_and(|c| c.is_ascii_alphabetic() || c == '_');
            let rest_ok = chars.all(|c| c.is_ascii_alphanumeric() || c == '_');
            return if first_ok && rest_ok {
                Ok(SecretRef::Env(name.to_owned()))
            } else {
                Err(SecretRefError::InvalidEnvName)
            };
        }
        if let Some(path) = s.strip_prefix("file:") {
            return if path.is_empty() {
                Err(SecretRefError::EmptyFilePath)
            } else {
                Ok(SecretRef::File(PathBuf::from(path)))
            };
        }
        Err(SecretRefError::NotAReference)
    }
}

impl Serialize for SecretRef {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.collect_str(self)
    }
}

impl<'de> Deserialize<'de> for SecretRef {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let s = String::deserialize(deserializer)?;
        s.parse().map_err(serde::de::Error::custom)
    }
}

impl JsonSchema for SecretRef {
    fn schema_name() -> std::borrow::Cow<'static, str> {
        "SecretRef".into()
    }

    fn json_schema(_: &mut SchemaGenerator) -> Schema {
        crate::schema::string_schema(
            "Reference to a secret, never the secret itself: 'env:<VARIABLE>' or 'file:<path>'.",
            Some("^(env:[A-Za-z_][A-Za-z0-9_]*|file:.+)$"),
            &["env:GEMINI_API_KEY", "file:/run/secrets/gemini_api_key"],
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const FAKE_KEY: &str = "AIzaSyFAKE-not-a-real-key-0123456789";

    #[test]
    fn parses_references() {
        assert_eq!(
            "env:GEMINI_API_KEY".parse::<SecretRef>().unwrap(),
            SecretRef::Env("GEMINI_API_KEY".into())
        );
        assert_eq!(
            "file:/run/secrets/k".parse::<SecretRef>().unwrap(),
            SecretRef::File("/run/secrets/k".into())
        );
        assert_eq!(
            "env:_X1".parse::<SecretRef>().unwrap().to_string(),
            "env:_X1"
        );
    }

    #[test]
    fn rejects_literals_without_echoing_them() {
        let err = FAKE_KEY.parse::<SecretRef>().unwrap_err();
        assert_eq!(err, SecretRefError::NotAReference);
        assert!(!err.to_string().contains(FAKE_KEY));

        let err = format!("env:{FAKE_KEY}").parse::<SecretRef>().unwrap_err();
        assert_eq!(err, SecretRefError::InvalidEnvName);
        assert!(!err.to_string().contains(FAKE_KEY));

        let json = format!("\"{FAKE_KEY}\"");
        let err = serde_json::from_str::<SecretRef>(&json).unwrap_err();
        assert!(!err.to_string().contains(FAKE_KEY));
    }

    #[test]
    fn rejects_empty_parts() {
        assert_eq!(
            "env:".parse::<SecretRef>(),
            Err(SecretRefError::InvalidEnvName)
        );
        assert_eq!(
            "env:1X".parse::<SecretRef>(),
            Err(SecretRefError::InvalidEnvName)
        );
        assert_eq!(
            "file:".parse::<SecretRef>(),
            Err(SecretRefError::EmptyFilePath)
        );
    }
}
