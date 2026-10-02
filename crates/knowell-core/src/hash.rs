use std::fmt;
use std::str::FromStr;

use schemars::{JsonSchema, Schema, SchemaGenerator};
use serde::{Deserialize, Deserializer, Serialize, Serializer};

/// BLAKE3 hash identifying a piece of content (a file blob, a prepared
/// embedding input, …). Displayed and serialised as 64 lowercase hex digits.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct ContentHash([u8; 32]);

/// Error returned when parsing a [`ContentHash`] from text.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("content hash must be 64 lowercase hexadecimal digits")]
pub struct ContentHashError;

impl ContentHash {
    /// Hashes `bytes`.
    pub fn of(bytes: &[u8]) -> Self {
        Self(*blake3::hash(bytes).as_bytes())
    }

    /// Hashes several byte slices as one logical input, each prefixed with
    /// its length so that `["ab", "c"]` and `["a", "bc"]` differ.
    pub fn of_parts<'a>(parts: impl IntoIterator<Item = &'a [u8]>) -> Self {
        let mut hasher = blake3::Hasher::new();
        for part in parts {
            hasher.update(&(part.len() as u64).to_le_bytes());
            hasher.update(part);
        }
        Self(*hasher.finalize().as_bytes())
    }

    /// Wraps a digest previously obtained from [`ContentHash::as_bytes`]
    /// (e.g. read back from storage). No hashing happens here.
    pub fn from_bytes(bytes: [u8; 32]) -> Self {
        Self(bytes)
    }

    /// Raw digest bytes.
    pub fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }

    /// The first 12 hex digits, for logs and UI where the full hash is noise.
    pub fn short(&self) -> String {
        let mut s = self.to_string();
        s.truncate(12);
        s
    }
}

impl fmt::Display for ContentHash {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for byte in &self.0 {
            write!(f, "{byte:02x}")?;
        }
        Ok(())
    }
}

impl fmt::Debug for ContentHash {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "ContentHash({})", self.short())
    }
}

impl FromStr for ContentHash {
    type Err = ContentHashError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let bytes = s.as_bytes();
        if bytes.len() != 64 {
            return Err(ContentHashError);
        }
        let mut out = [0u8; 32];
        let (pairs, _) = bytes.as_chunks::<2>();
        for (slot, [hi, lo]) in out.iter_mut().zip(pairs) {
            *slot = (hex_value(*hi)? << 4) | hex_value(*lo)?;
        }
        Ok(Self(out))
    }
}

fn hex_value(c: u8) -> Result<u8, ContentHashError> {
    match c {
        b'0'..=b'9' => Ok(c - b'0'),
        b'a'..=b'f' => Ok(c - b'a' + 10),
        _ => Err(ContentHashError),
    }
}

impl Serialize for ContentHash {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.collect_str(self)
    }
}

impl<'de> Deserialize<'de> for ContentHash {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let s = String::deserialize(deserializer)?;
        s.parse().map_err(serde::de::Error::custom)
    }
}

impl JsonSchema for ContentHash {
    fn schema_name() -> std::borrow::Cow<'static, str> {
        "ContentHash".into()
    }

    fn json_schema(_: &mut SchemaGenerator) -> Schema {
        crate::schema::string_schema("BLAKE3 content hash (hex).", Some("^[0-9a-f]{64}$"), &[])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_through_text() {
        let h = ContentHash::of(b"hello");
        let text = h.to_string();
        assert_eq!(text.len(), 64);
        assert_eq!(text.parse::<ContentHash>().unwrap(), h);
        assert_eq!(h.short().len(), 12);
    }

    #[test]
    fn rejects_bad_text() {
        assert!("abc".parse::<ContentHash>().is_err());
        assert!("G".repeat(64).parse::<ContentHash>().is_err());
        assert!(
            "A".repeat(64).parse::<ContentHash>().is_err(),
            "uppercase is not canonical"
        );
    }

    #[test]
    fn bytes_round_trip() {
        let h = ContentHash::of(b"abc");
        assert_eq!(ContentHash::from_bytes(*h.as_bytes()), h);
    }

    #[test]
    fn parts_are_length_prefixed() {
        let a = ContentHash::of_parts([b"ab".as_slice(), b"c".as_slice()]);
        let b = ContentHash::of_parts([b"a".as_slice(), b"bc".as_slice()]);
        assert_ne!(a, b);
    }

    #[test]
    fn serde_uses_hex_string() {
        let h = ContentHash::of(b"x");
        let json = serde_json::to_string(&h).unwrap();
        assert_eq!(json, format!("\"{h}\""));
        assert_eq!(serde_json::from_str::<ContentHash>(&json).unwrap(), h);
    }
}
