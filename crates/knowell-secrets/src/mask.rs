//! Output masking of *known* secret values.
//!
//! Unlike [`crate::scan()`], which guesses from shape, a [`Masker`] knows the
//! exact secrets (for example API keys resolved from configuration) and
//! removes every occurrence from arbitrary text: log lines, error messages,
//! MCP and HTTP responses.

use std::borrow::Cow;

use secrecy::{ExposeSecret, SecretString};

/// Replacement for every masked occurrence.
pub const MASK: &str = "[REDACTED]";

/// Secrets shorter than this many **bytes** are ignored: masking 1-5 byte
/// strings would corrupt ordinary text ("true", "1234") and gives no real
/// protection, since such values are not credible secrets.
pub const MIN_SECRET_LEN: usize = 6;

/// Holds known secret values and masks them in text.
#[derive(Default)]
pub struct Masker {
    /// Distinct values, longest first.
    values: Vec<SecretString>,
}

impl Masker {
    /// Creates an empty masker.
    pub fn new() -> Self {
        Self::default()
    }

    /// Builds a masker from values (see [`Masker::add`] for the filtering).
    pub fn from_values(values: impl IntoIterator<Item = SecretString>) -> Self {
        let mut masker = Self::new();
        for value in values {
            masker.add(value);
        }
        masker
    }

    /// Registers a secret. Values shorter than [`MIN_SECRET_LEN`] bytes and
    /// duplicates are ignored. Returns whether the value was registered.
    pub fn add(&mut self, value: SecretString) -> bool {
        let text = value.expose_secret();
        if text.len() < MIN_SECRET_LEN || self.values.iter().any(|v| v.expose_secret() == text) {
            return false;
        }
        self.values.push(value);
        // Longest first so that a secret containing another is masked whole.
        self.values
            .sort_by_key(|v| std::cmp::Reverse(v.expose_secret().len()));
        true
    }

    /// Number of registered secrets.
    pub fn len(&self) -> usize {
        self.values.len()
    }

    /// Whether no secret is registered.
    pub fn is_empty(&self) -> bool {
        self.values.is_empty()
    }

    /// Replaces every occurrence of every known secret with [`MASK`].
    ///
    /// Occurrences that overlap or touch are merged into one mask, so a
    /// secret that partially overlaps another leaves no fragment behind.
    /// Returns the input unchanged (borrowed) when nothing matched.
    pub fn mask<'a>(&self, text: &'a str) -> Cow<'a, str> {
        let mut spans: Vec<(usize, usize)> = Vec::new();
        for value in &self.values {
            let needle = value.expose_secret();
            spans.extend(
                text.match_indices(needle)
                    .map(|(start, m)| (start, start + m.len())),
            );
        }
        if spans.is_empty() {
            return Cow::Borrowed(text);
        }
        spans.sort_unstable();
        let mut merged: Vec<(usize, usize)> = Vec::with_capacity(spans.len());
        for (start, end) in spans {
            match merged.last_mut() {
                Some(last) if start <= last.1 => last.1 = last.1.max(end),
                _ => merged.push((start, end)),
            }
        }
        let mut out = String::with_capacity(text.len());
        let mut cursor = 0;
        for (start, end) in merged {
            if let Some(kept) = text.get(cursor..start) {
                out.push_str(kept);
            }
            out.push_str(MASK);
            cursor = end;
        }
        if let Some(rest) = text.get(cursor..) {
            out.push_str(rest);
        }
        Cow::Owned(out)
    }
}

impl std::fmt::Debug for Masker {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Masker")
            .field("secrets", &self.values.len())
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(v: &str) -> SecretString {
        SecretString::from(v.to_owned())
    }

    #[test]
    fn masks_all_occurrences() {
        let m = Masker::from_values([s("KNOWELL_CANARY_abc123")]);
        let out = m.mask("a KNOWELL_CANARY_abc123 b KNOWELL_CANARY_abc123");
        assert_eq!(out, "a [REDACTED] b [REDACTED]");
    }

    #[test]
    fn borrows_when_nothing_matches() {
        let m = Masker::from_values([s("KNOWELL_CANARY_abc123")]);
        assert!(matches!(m.mask("nothing here"), Cow::Borrowed(_)));
        assert!(matches!(Masker::new().mask("x"), Cow::Borrowed(_)));
    }

    #[test]
    fn short_values_and_duplicates_are_ignored() {
        let mut m = Masker::new();
        assert!(!m.add(s("short")));
        assert!(!m.add(s("")));
        assert!(m.add(s("sixsix")));
        assert!(!m.add(s("sixsix")));
        assert_eq!(m.len(), 1);
        assert_eq!(m.mask("short sixsix"), "short [REDACTED]");
    }

    #[test]
    fn longest_secret_wins_when_nested() {
        let m = Masker::from_values([s("secret1"), s("secret1-and-more")]);
        assert_eq!(m.mask("x secret1-and-more y"), "x [REDACTED] y");
    }

    #[test]
    fn partial_overlap_leaves_no_fragment() {
        let m = Masker::from_values([s("abcdef"), s("defghi")]);
        assert_eq!(m.mask("--abcdefghi--"), "--[REDACTED]--");
    }

    #[test]
    fn secret_equal_to_mask_text_does_not_loop() {
        let m = Masker::from_values([s("REDACTED")]);
        assert_eq!(m.mask("REDACTED REDACTED"), "[REDACTED] [REDACTED]");
    }

    #[test]
    fn unicode_is_safe() {
        let m = Masker::from_values([s("sécrét-ключ-密钥")]);
        let text = "préfixe sécrét-ключ-密钥 suffixe ✓";
        assert_eq!(m.mask(text), "préfixe [REDACTED] suffixe ✓");
    }

    #[test]
    fn debug_does_not_reveal_values() {
        let m = Masker::from_values([s("KNOWELL_CANARY_dbg999")]);
        let d = format!("{m:?}");
        assert!(!d.contains("CANARY"));
        assert!(d.contains('1'));
    }

    #[test]
    fn handles_huge_input() {
        let m = Masker::from_values([s("needle-value")]);
        let text = format!(
            "{}needle-value{}",
            "x".repeat(1_000_000),
            "y".repeat(1_000_000)
        );
        let out = m.mask(&text);
        assert_eq!(out.len(), 2_000_000 + MASK.len());
    }
}
