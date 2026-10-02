//! Content classification shared by the filesystem walker ([`crate::fs`])
//! and the git-object walker ([`crate::git`]), so that a file read from a
//! checkout and the same blob read from git objects are treated identically.

use knowell_core::{ContentHash, RepoPath};
use knowell_secrets::scan::redact;

use crate::fs::{SkipReason, SourceFile};

/// How many leading bytes are inspected for NUL when detecting binary files.
pub(crate) const BINARY_SNIFF_BYTES: usize = 8 * 1024;
/// A UTF-8 byte order mark; stripped before the text is decoded.
pub(crate) const UTF8_BOM: [u8; 3] = [0xEF, 0xBB, 0xBF];

/// The result of reading one file or blob.
pub(crate) enum Outcome {
    /// A readable text file, already redacted.
    File(Box<SourceFile>),
    /// The file was not returned.
    Skip(SkipReason),
}

/// Classifies bytes that were already read and already checked against the
/// size limit: binary check (NUL in the first 8 KiB), UTF-8 validation
/// (a leading BOM is stripped), then secret redaction.
///
/// The hash covers the original bytes (BOM and secrets included) so that an
/// edit to redacted content is still detected; the reported size is
/// `bytes.len()`.
pub(crate) fn decode(path: RepoPath, bytes: &[u8]) -> Outcome {
    if bytes.iter().take(BINARY_SNIFF_BYTES).any(|&b| b == 0) {
        return Outcome::Skip(SkipReason::Binary);
    }
    let hash = ContentHash::of(bytes);
    let body = bytes.strip_prefix(&UTF8_BOM).unwrap_or(bytes);
    let Ok(text) = std::str::from_utf8(body) else {
        return Outcome::Skip(SkipReason::NotUtf8);
    };
    let redacted = redact(text);
    Outcome::File(Box::new(SourceFile {
        path,
        text: redacted.text,
        hash,
        size: bytes.len() as u64,
        redactions: redacted.findings,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(s: &str) -> RepoPath {
        RepoPath::new(s).unwrap()
    }

    #[test]
    fn classifies_binary_utf8_and_text() {
        assert!(matches!(
            decode(p("a"), b"a\0b"),
            Outcome::Skip(SkipReason::Binary)
        ));
        assert!(matches!(
            decode(p("a"), &[0xC3, 0x28]),
            Outcome::Skip(SkipReason::NotUtf8)
        ));
        let Outcome::File(file) = decode(p("a"), b"\xEF\xBB\xBFhi\n") else {
            panic!("expected a file");
        };
        assert_eq!(file.text, "hi\n");
        assert_eq!(file.size, 6);
        assert_eq!(file.hash, ContentHash::of(b"\xEF\xBB\xBFhi\n"));
    }

    #[test]
    fn truncated_and_empty_input() {
        let Outcome::File(empty) = decode(p("e"), b"") else {
            panic!("empty input is an empty text file");
        };
        assert_eq!(empty.text, "");
        // A multi-byte character cut in half is not UTF-8.
        let cut = "日本".as_bytes();
        assert!(matches!(
            decode(p("c"), &cut[..4]),
            Outcome::Skip(SkipReason::NotUtf8)
        ));
        // A lone BOM decodes to empty text.
        let Outcome::File(bom) = decode(p("b"), &UTF8_BOM) else {
            panic!("a lone BOM is text");
        };
        assert_eq!(bom.text, "");
        assert_eq!(bom.size, 3);
    }
}
