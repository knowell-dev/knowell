//! The documents a retriever searches.

use knowell_core::ContentHash;

use crate::error::EvalError;
use crate::fixture::FixtureInfo;

/// One searchable document. Relevance is judged per file for now, so a
/// document is a whole file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CorpusDoc {
    /// `<project>/<path>`, e.g. `billing-api/src/main.ts`.
    pub id: String,
    /// Full text of the file.
    pub text: String,
}

/// A set of documents with unique ids, kept sorted by id.
///
/// A corpus records which fixture it was built from (if any) so reports can
/// refuse to compare runs over different workspaces.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Corpus {
    docs: Vec<CorpusDoc>,
    fixture: Option<FixtureInfo>,
}

impl Corpus {
    /// Builds a corpus from any documents; rejects duplicate ids.
    pub fn from_docs(docs: impl IntoIterator<Item = CorpusDoc>) -> Result<Self, EvalError> {
        let mut docs: Vec<CorpusDoc> = docs.into_iter().collect();
        docs.sort_by(|a, b| a.id.cmp(&b.id));
        if let Some(pair) = docs
            .windows(2)
            .find(|w| matches!(w, [a, b] if a.id == b.id))
            && let Some(first) = pair.first()
        {
            return Err(EvalError::DuplicateDoc(first.id.clone()));
        }
        Ok(Self {
            docs,
            fixture: None,
        })
    }

    /// For callers that guarantee unique ids by construction.
    pub(crate) fn from_unique_sorted(mut docs: Vec<CorpusDoc>) -> Self {
        docs.sort_by(|a, b| a.id.cmp(&b.id));
        docs.dedup_by(|a, b| a.id == b.id);
        Self {
            docs,
            fixture: None,
        }
    }

    /// Records the fixture this corpus was built from (e.g. by a walker
    /// over a written fixture).
    pub fn with_fixture(mut self, info: FixtureInfo) -> Self {
        self.fixture = Some(info);
        self
    }

    /// Documents sorted by id.
    pub fn docs(&self) -> &[CorpusDoc] {
        &self.docs
    }

    /// The source fixture, if known.
    pub fn fixture(&self) -> Option<&FixtureInfo> {
        self.fixture.as_ref()
    }

    /// Number of documents.
    pub fn len(&self) -> usize {
        self.docs.len()
    }

    /// True when there are no documents.
    pub fn is_empty(&self) -> bool {
        self.docs.is_empty()
    }

    /// True when a document with this id exists.
    pub fn contains(&self, id: &str) -> bool {
        self.docs
            .binary_search_by(|d| d.id.as_str().cmp(id))
            .is_ok()
    }

    /// BLAKE3 over all `(id, text)` pairs in id order.
    pub fn hash(&self) -> ContentHash {
        let tag: &[u8] = b"knowell-eval/corpus/v1";
        let parts = std::iter::once(tag).chain(
            self.docs
                .iter()
                .flat_map(|d| [d.id.as_bytes(), d.text.as_bytes()]),
        );
        ContentHash::of_parts(parts)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn doc(id: &str, text: &str) -> CorpusDoc {
        CorpusDoc {
            id: id.to_owned(),
            text: text.to_owned(),
        }
    }

    #[test]
    fn sorts_and_rejects_duplicates() {
        let corpus = Corpus::from_docs([doc("b/x", "2"), doc("a/y", "1")]).unwrap();
        let ids: Vec<&str> = corpus.docs().iter().map(|d| d.id.as_str()).collect();
        assert_eq!(ids, ["a/y", "b/x"]);
        assert!(corpus.contains("b/x"));
        assert!(!corpus.contains("c/z"));
        let err = Corpus::from_docs([doc("a/x", "1"), doc("a/x", "2")]).unwrap_err();
        assert!(matches!(err, EvalError::DuplicateDoc(id) if id == "a/x"));
        assert!(Corpus::from_docs([]).unwrap().is_empty());
    }

    #[test]
    fn hash_depends_on_content_not_input_order() {
        let a = Corpus::from_docs([doc("a/1", "x"), doc("b/2", "y")]).unwrap();
        let b = Corpus::from_docs([doc("b/2", "y"), doc("a/1", "x")]).unwrap();
        let c = Corpus::from_docs([doc("a/1", "x"), doc("b/2", "z")]).unwrap();
        assert_eq!(a.hash(), b.hash());
        assert_ne!(a.hash(), c.hash());
    }
}
