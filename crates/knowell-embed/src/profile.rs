//! Embedding profiles: the identity of a vector space.

use knowell_core::ContentHash;

use crate::embedding::DocumentInput;

/// Which kind of provider produces a profile's vectors.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum ProviderKind {
    /// Google Gemini embedding models (`batchEmbedContents`).
    Gemini,
    /// Any server speaking the OpenAI `/v1/embeddings` protocol.
    OpenAiCompatible,
    /// Ollama's `/api/embed`.
    Ollama,
    /// The deterministic feature-hashing embedder used by tests and CI.
    Fake,
}

impl ProviderKind {
    /// Stable lowercase name, used in profile keys, logs and errors.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Gemini => "gemini",
            Self::OpenAiCompatible => "openai-compatible",
            Self::Ollama => "ollama",
            Self::Fake => "fake",
        }
    }
}

/// Everything that makes two vectors comparable.
///
/// Vectors from different profiles must never be compared or mixed in one
/// index: a different model, dimensionality or input format (prefixes, title
/// handling) produces a different space even if the numbers look alike.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct EmbeddingProfile {
    /// Provider family.
    pub provider_kind: ProviderKind,
    /// Model identifier as the provider names it, for example `gemini-embedding-2`.
    pub model: String,
    /// Number of components of every vector in this profile.
    pub dimensions: u32,
    /// Version of Knowell's input preparation (prefixes, title layout).
    /// Bump it whenever the prepared text for the same chunk changes.
    pub input_format_version: u32,
}

impl EmbeddingProfile {
    /// Stable identity of the profile: a BLAKE3 hash over its fields, length
    /// prefixed so field boundaries cannot be confused. It never changes
    /// between releases or platforms for the same field values.
    pub fn profile_key(&self) -> ContentHash {
        ContentHash::of_parts([
            b"knowell.embedding-profile.v1".as_slice(),
            self.provider_kind.as_str().as_bytes(),
            self.model.as_bytes(),
            &self.dimensions.to_le_bytes(),
            &self.input_format_version.to_le_bytes(),
        ])
    }
}

/// Cache key of one embedding: the hash of the prepared input (content plus
/// context header) combined with the profile, so a vector cached under one
/// profile is never served for another.
pub fn cache_key(profile: &EmbeddingProfile, prepared_input_hash: ContentHash) -> ContentHash {
    ContentHash::of_parts([
        b"knowell.embedding-cache.v1".as_slice(),
        profile.profile_key().as_bytes().as_slice(),
        prepared_input_hash.as_bytes().as_slice(),
    ])
}

/// Version of the input preparation implemented in this crate.
pub const INPUT_FORMAT_VERSION: u32 = 1;

/// Query prefix of Gemini Embedding 2 for code retrieval.
pub const GEMINI_QUERY_PREFIX: &str = "task: code retrieval | query: ";

/// The exact text sent to the provider for `doc`.
///
/// Gemini Embedding 2 has no `task_type`; the task is expressed in the text:
/// `title: {title|none} | text: {text}`. Other providers receive the title
/// (when present) on its own line before the text.
pub fn prepare_document(kind: ProviderKind, doc: &DocumentInput) -> String {
    let title = doc.title.as_deref().filter(|t| !t.trim().is_empty());
    match kind {
        ProviderKind::Gemini => {
            format!("title: {} | text: {}", title.unwrap_or("none"), doc.text)
        }
        _ => match title {
            Some(t) => format!("{t}\n\n{}", doc.text),
            None => doc.text.clone(),
        },
    }
}

/// The exact text sent to the provider for a search query.
pub fn prepare_query(kind: ProviderKind, query: &str) -> String {
    match kind {
        ProviderKind::Gemini => format!("{GEMINI_QUERY_PREFIX}{query}"),
        _ => query.to_owned(),
    }
}

/// Hash of [`prepare_document`] output; feed it to [`cache_key`].
pub fn prepared_document_hash(kind: ProviderKind, doc: &DocumentInput) -> ContentHash {
    ContentHash::of(prepare_document(kind, doc).as_bytes())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn profile(dims: u32) -> EmbeddingProfile {
        EmbeddingProfile {
            provider_kind: ProviderKind::Gemini,
            model: "gemini-embedding-2".into(),
            dimensions: dims,
            input_format_version: 1,
        }
    }

    #[test]
    fn profile_key_is_stable_and_field_sensitive() {
        let base = profile(768);
        assert_eq!(base.profile_key(), profile(768).profile_key());
        assert_ne!(base.profile_key(), profile(1536).profile_key());
        let mut other = base.clone();
        other.input_format_version = 2;
        assert_ne!(base.profile_key(), other.profile_key());
        let mut other = base.clone();
        other.provider_kind = ProviderKind::Fake;
        assert_ne!(base.profile_key(), other.profile_key());
        let mut other = base.clone();
        other.model.push('x');
        assert_ne!(base.profile_key(), other.profile_key());
    }

    #[test]
    fn cache_key_depends_on_profile_and_input() {
        let h1 = ContentHash::of(b"a");
        let h2 = ContentHash::of(b"b");
        assert_eq!(cache_key(&profile(768), h1), cache_key(&profile(768), h1));
        assert_ne!(cache_key(&profile(768), h1), cache_key(&profile(768), h2));
        assert_ne!(cache_key(&profile(768), h1), cache_key(&profile(1536), h1));
    }

    #[test]
    fn gemini_preparation_uses_prefixes() {
        let doc = DocumentInput::with_title("src/lib.rs", "fn main() {}");
        assert_eq!(
            prepare_document(ProviderKind::Gemini, &doc),
            "title: src/lib.rs | text: fn main() {}"
        );
        let untitled = DocumentInput::new("x");
        assert_eq!(
            prepare_document(ProviderKind::Gemini, &untitled),
            "title: none | text: x"
        );
        let blank = DocumentInput::with_title("  ", "x");
        assert_eq!(
            prepare_document(ProviderKind::Gemini, &blank),
            "title: none | text: x"
        );
        assert_eq!(
            prepare_query(ProviderKind::Gemini, "find it"),
            "task: code retrieval | query: find it"
        );
    }

    #[test]
    fn other_providers_get_plain_text() {
        let doc = DocumentInput::with_title("t", "body");
        assert_eq!(prepare_document(ProviderKind::Ollama, &doc), "t\n\nbody");
        assert_eq!(prepare_query(ProviderKind::OpenAiCompatible, "q"), "q");
    }
}
