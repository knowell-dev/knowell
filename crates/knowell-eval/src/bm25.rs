//! BM25 retriever over `knowell-lexical`, the engine's real lexical index.

use knowell_lexical::{LexicalDoc, LexicalIndex, SearchOptions};

use crate::corpus::Corpus;
use crate::error::EvalError;
use crate::retriever::{RankedDoc, Retriever};

/// Ranks documents with Knowell's code-aware tokenizer and Tantivy BM25,
/// indexed in RAM. Document id and path are the corpus id
/// (`<project>/<path>`), so project names also match as path terms.
pub struct Bm25Retriever {
    index: LexicalIndex,
    options: SearchOptions,
}

impl Bm25Retriever {
    /// Report row name.
    pub const NAME: &'static str = "bm25";

    /// Indexes every corpus document; searches use the lexical index's
    /// default options.
    pub fn new(corpus: &Corpus) -> Result<Self, EvalError> {
        Self::with_options(corpus, SearchOptions::default())
    }

    /// Like [`new`](Self::new) with explicit search options (for tuning
    /// experiments; report rows stay named `bm25`).
    pub fn with_options(corpus: &Corpus, options: SearchOptions) -> Result<Self, EvalError> {
        let err = |e: knowell_lexical::LexicalError| EvalError::Retriever {
            retriever: Self::NAME.to_owned(),
            message: e.to_string(),
        };
        let index = LexicalIndex::create_in_ram().map_err(err)?;
        let mut writer = index.writer().map_err(err)?;
        for doc in corpus.docs() {
            writer
                .add(LexicalDoc {
                    id: &doc.id,
                    path: &doc.id,
                    text: &doc.text,
                })
                .map_err(err)?;
        }
        writer.commit().map_err(err)?;
        Ok(Self { index, options })
    }
}

impl Retriever for Bm25Retriever {
    fn name(&self) -> &str {
        Self::NAME
    }

    fn search(&self, query: &str, k: usize) -> Result<Vec<RankedDoc>, EvalError> {
        let hits = self
            .index
            .search_with(query, k, &self.options)
            .map_err(|e| EvalError::Retriever {
                retriever: Self::NAME.to_owned(),
                message: e.to_string(),
            })?;
        Ok(hits
            .into_iter()
            .map(|hit| RankedDoc {
                id: hit.id,
                score: f64::from(hit.score),
            })
            .collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::corpus::CorpusDoc;

    #[test]
    fn ranks_identifier_matches_first() {
        let corpus = Corpus::from_docs([
            CorpusDoc {
                id: "billing-api/src/subscription.service.ts".into(),
                text: "export class SubscriptionService { async cancelSubscription(id) {} }".into(),
            },
            CorpusDoc {
                id: "storefront-web/src/analytics.ts".into(),
                text: "track('subscription viewed')".into(),
            },
        ])
        .unwrap();
        let bm25 = Bm25Retriever::new(&corpus).unwrap();
        let hits = bm25.search("cancel subscription", 10).unwrap();
        assert_eq!(
            hits.first().map(|h| h.id.as_str()),
            Some("billing-api/src/subscription.service.ts")
        );
        assert!(bm25.search("", 10).unwrap().is_empty());
    }
}
