//! The Tantivy-backed BM25 index.

use std::collections::BTreeSet;
use std::path::Path;

use tantivy::collector::TopDocs;
use tantivy::query::{BooleanQuery, BoostQuery, Occur, Query, TermQuery};
use tantivy::schema::{
    Field, IndexRecordOption, STORED, STRING, Schema, TextFieldIndexing, TextOptions, Value,
};
use tantivy::tokenizer::{PreTokenizedString, Token as TantivyToken};
use tantivy::{
    DocAddress, DocId, DocSet, Index, IndexReader, IndexWriter, InvertedIndexReader, ReloadPolicy,
    Searcher, TantivyDocument, TantivyError, Term,
};

use crate::tokenize::{Token, tokenize, tokenize_query};

/// Score multiplier applied to matches in the `path` field.
///
/// A query term found in a file's path says more about the file's topic than
/// the same term appearing once in its body, so path matches count 1.5 times
/// as much as body matches of the same BM25 weight.
pub const PATH_BOOST: f32 = 1.5;

/// Memory budget of the single indexing thread, in bytes.
const WRITER_MEMORY_BYTES: usize = 50_000_000;

/// Errors produced by the lexical index.
#[derive(Debug, thiserror::Error)]
pub enum LexicalError {
    /// Tantivy reported an error while performing `context`.
    #[error("lexical index: {context}: {source}")]
    Tantivy {
        /// What the index was doing, for example `"committing"`.
        context: &'static str,
        /// The underlying Tantivy error.
        #[source]
        source: TantivyError,
    },
    /// A filesystem operation failed.
    #[error("lexical index: {context}: {source}")]
    Io {
        /// What the index was doing.
        context: &'static str,
        /// The underlying I/O error.
        #[source]
        source: std::io::Error,
    },
    /// The opened index does not have the schema this crate writes.
    #[error("lexical index: schema is missing field `{0}`; the index was not created by knowell")]
    SchemaMismatch(&'static str),
    /// A stored field that every document must have was absent.
    #[error("lexical index: stored field `{0}` is missing from a matching document")]
    MissingStoredField(&'static str),
}

fn tantivy_err(context: &'static str) -> impl FnOnce(TantivyError) -> LexicalError {
    move |source| LexicalError::Tantivy { context, source }
}

#[derive(Clone, Copy)]
struct Fields {
    id: Field,
    path_text: Field,
    path: Field,
    body: Field,
}

impl Fields {
    fn build_schema() -> Schema {
        let mut builder = Schema::builder();
        // Pre-tokenized fields: the "raw" tokenizer is never invoked because
        // documents arrive as `PreTokenizedString`; it only names the field.
        let tokens = TextOptions::default().set_indexing_options(
            TextFieldIndexing::default()
                .set_tokenizer("raw")
                .set_index_option(IndexRecordOption::WithFreqs),
        );
        builder.add_text_field("id", STRING | STORED);
        builder.add_text_field("path_text", STORED);
        builder.add_text_field("path", tokens.clone());
        builder.add_text_field("body", tokens);
        builder.build()
    }

    fn from_schema(schema: &Schema) -> Result<Self, LexicalError> {
        let get = |name: &'static str| {
            schema
                .get_field(name)
                .map_err(|_| LexicalError::SchemaMismatch(name))
        };
        Ok(Self {
            id: get("id")?,
            path_text: get("path_text")?,
            path: get("path")?,
            body: get("body")?,
        })
    }
}

/// A document to index.
#[derive(Debug, Clone, Copy)]
pub struct LexicalDoc<'a> {
    /// Unique document key (for example a file path or chunk id). Returned in
    /// hits and used by [`LexicalWriter::delete`].
    pub id: &'a str,
    /// Display path of the document; tokenized and searched with
    /// [`PATH_BOOST`], and returned verbatim in hits.
    pub path: &'a str,
    /// Searchable text. It is tokenized but never stored.
    pub text: &'a str,
}

/// One search result.
#[derive(Debug, Clone, PartialEq)]
pub struct LexicalHit {
    /// The document key given at indexing time.
    pub id: String,
    /// The path given at indexing time.
    pub path: String,
    /// BM25 score (higher is better), after the optional coordination
    /// factor; only comparable within one result list.
    pub score: f32,
    /// Normalised query terms found in this document's path or body, sorted.
    /// Used to explain lexical matches ("matched: cancel, subscription").
    pub matched_terms: Vec<String>,
}

/// Options for [`LexicalIndex::search_with`].
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SearchOptions {
    /// Exponent of the coordination factor `(matched terms / query terms)^c`
    /// multiplied into each BM25 score. `0.0` disables it (plain BM25).
    ///
    /// BM25 sums per-term weights, so one rare term can outrank a document
    /// that matches every term of the question; coordination rewards
    /// documents covering more of the query. The default is chosen by
    /// measurement on the evaluation set, not by intuition.
    pub coordination: f32,
}

/// Default coordination exponent. Measured on the synthetic `acme-goods`
/// evaluation set (seed 42, small): exponents 0 / 0.5 / 1 / 2 gave
/// nDCG@10 0.457 / 0.474 / 0.482 / 0.478 and Recall@10 0.562 / 0.580 /
/// 0.599 / 0.593, so 1.0 (classic coordination) is the default. Re-measure
/// when the tokenizer or the evaluation set changes.
pub const DEFAULT_COORDINATION: f32 = 1.0;

impl Default for SearchOptions {
    fn default() -> Self {
        Self {
            coordination: DEFAULT_COORDINATION,
        }
    }
}

/// A code-aware BM25 index over `path` and `body` text.
///
/// Searching is `&self`; indexing goes through one [`LexicalWriter`] at a time.
pub struct LexicalIndex {
    index: Index,
    reader: IndexReader,
    fields: Fields,
}

fn pre_tokenized(tokens: &[Token]) -> PreTokenizedString {
    PreTokenizedString {
        // The original text is only needed for stored fields; these are not.
        text: String::new(),
        tokens: tokens
            .iter()
            .map(|t| TantivyToken {
                offset_from: t.offset_from,
                offset_to: t.offset_to,
                position: t.position,
                text: t.text.clone(),
                position_length: 1,
            })
            .collect(),
    }
}

impl LexicalIndex {
    fn from_index(index: Index) -> Result<Self, LexicalError> {
        let fields = Fields::from_schema(&index.schema())?;
        let reader = index
            .reader_builder()
            .reload_policy(ReloadPolicy::Manual)
            .try_into()
            .map_err(tantivy_err("opening reader"))?;
        Ok(Self {
            index,
            reader,
            fields,
        })
    }

    /// Creates an empty index in memory.
    ///
    /// # Errors
    /// Returns an error if the reader cannot be created.
    pub fn create_in_ram() -> Result<Self, LexicalError> {
        Self::from_index(Index::create_in_ram(Fields::build_schema()))
    }

    /// Creates an empty index in `dir`, creating the directory if needed.
    ///
    /// # Errors
    /// Fails if the directory cannot be created or already contains an index.
    pub fn create_in_dir(dir: &Path) -> Result<Self, LexicalError> {
        std::fs::create_dir_all(dir).map_err(|source| LexicalError::Io {
            context: "creating index directory",
            source,
        })?;
        let index = Index::create_in_dir(dir, Fields::build_schema())
            .map_err(tantivy_err("creating index"))?;
        Self::from_index(index)
    }

    /// Opens an existing index in `dir`.
    ///
    /// # Errors
    /// Fails if `dir` holds no index or the index has a foreign schema.
    pub fn open_in_dir(dir: &Path) -> Result<Self, LexicalError> {
        let index = Index::open_in_dir(dir).map_err(tantivy_err("opening index"))?;
        Self::from_index(index)
    }

    /// Starts a single-threaded writer.
    ///
    /// One thread keeps the segment layout (and therefore tie-breaking inside
    /// Tantivy) deterministic. Only one writer can exist per index directory.
    ///
    /// # Errors
    /// Fails if another writer holds the index lock.
    pub fn writer(&self) -> Result<LexicalWriter, LexicalError> {
        let writer = self
            .index
            .writer_with_num_threads(1, WRITER_MEMORY_BYTES)
            .map_err(tantivy_err("creating writer"))?;
        Ok(LexicalWriter {
            writer,
            reader: self.reader.clone(),
            fields: self.fields,
        })
    }

    /// Number of live (non-deleted) documents visible to searches.
    #[must_use]
    pub fn num_docs(&self) -> u64 {
        self.reader.searcher().num_docs()
    }

    /// Runs `query` and returns at most `limit` hits, best first.
    ///
    /// The query is tokenized with [`tokenize_query`] (same normalisation as
    /// the index) and turned into a disjunction of term queries over `body`
    /// and `path`; it never goes through a query-language parser, so `:`,
    /// quotes and parentheses are plain separators. A query without tokens
    /// returns no hits. Hits are ordered by score descending, then `id`
    /// ascending, regardless of segment or document order.
    ///
    /// # Errors
    /// Fails if the index cannot be read.
    pub fn search(&self, query: &str, limit: usize) -> Result<Vec<LexicalHit>, LexicalError> {
        self.search_with(query, limit, &SearchOptions::default())
    }

    /// [`search`](Self::search) with explicit [`SearchOptions`].
    ///
    /// # Errors
    ///
    /// Fails when Tantivy cannot execute the query or load stored fields.
    pub fn search_with(
        &self,
        query: &str,
        limit: usize,
        options: &SearchOptions,
    ) -> Result<Vec<LexicalHit>, LexicalError> {
        if limit == 0 {
            return Ok(Vec::new());
        }
        let terms: BTreeSet<String> = tokenize_query(query).into_iter().map(|t| t.text).collect();
        if terms.is_empty() {
            return Ok(Vec::new());
        }
        let mut clauses: Vec<(Occur, Box<dyn Query>)> = Vec::with_capacity(terms.len() * 2);
        for term in &terms {
            let body = TermQuery::new(
                Term::from_field_text(self.fields.body, term),
                IndexRecordOption::WithFreqs,
            );
            let path = TermQuery::new(
                Term::from_field_text(self.fields.path, term),
                IndexRecordOption::WithFreqs,
            );
            clauses.push((Occur::Should, Box::new(body)));
            clauses.push((
                Occur::Should,
                Box::new(BoostQuery::new(Box::new(path), PATH_BOOST)),
            ));
        }
        let query = BooleanQuery::new(clauses);

        let searcher = self.reader.searcher();
        let total = usize::try_from(searcher.num_docs()).unwrap_or(usize::MAX);
        if total == 0 {
            return Ok(Vec::new());
        }

        // Tantivy breaks score ties by doc address. To make the cut at `limit`
        // independent of that, keep widening the fetch until the last fetched
        // score is strictly below the score at the cut (or everything was fetched).
        // Coordination reorders candidates, so it needs a wider window than
        // the final cut.
        let window = if options.coordination > 0.0 {
            limit.saturating_mul(4).max(64)
        } else {
            limit
        };
        let mut fetch = window.saturating_mul(2).max(16).min(total);
        let mut scored: Vec<(f32, DocAddress)>;
        loop {
            scored = searcher
                .search(&query, &TopDocs::with_limit(fetch).order_by_score())
                .map_err(tantivy_err("searching"))?;
            let exhausted = scored.len() < fetch || fetch >= total;
            let cut_score = scored.get(window.saturating_sub(1)).map(|s| s.0);
            let last_score = scored.last().map(|s| s.0);
            let settled = match (cut_score, last_score) {
                (Some(cut), Some(last)) => scored.len() > window && last < cut,
                _ => true,
            };
            if exhausted || settled {
                break;
            }
            fetch = fetch.saturating_mul(2).min(total);
        }

        let mut hits = Vec::with_capacity(scored.len());
        for (score, addr) in scored {
            let doc: TantivyDocument = searcher
                .doc(addr)
                .map_err(tantivy_err("loading stored fields"))?;
            let get = |field: Field, name: &'static str| {
                doc.get_first(field)
                    .and_then(|v| v.as_str().map(str::to_owned))
                    .ok_or(LexicalError::MissingStoredField(name))
            };
            let matched_terms = self.matched_terms(&searcher, addr, &terms)?;
            let score = if options.coordination > 0.0 {
                #[allow(clippy::cast_precision_loss)] // term counts are tiny
                let coverage = matched_terms.len() as f32 / terms.len() as f32;
                score * coverage.powf(options.coordination)
            } else {
                score
            };
            hits.push(LexicalHit {
                id: get(self.fields.id, "id")?,
                path: get(self.fields.path_text, "path_text")?,
                score,
                matched_terms,
            });
        }
        hits.sort_by(|a, b| b.score.total_cmp(&a.score).then_with(|| a.id.cmp(&b.id)));
        hits.truncate(limit);
        Ok(hits)
    }
    /// Which of `terms` occur in the document at `addr` (path or body).
    fn matched_terms(
        &self,
        searcher: &Searcher,
        addr: DocAddress,
        terms: &BTreeSet<String>,
    ) -> Result<Vec<String>, LexicalError> {
        let segment = searcher.segment_reader(addr.segment_ord);
        let body = segment
            .inverted_index(self.fields.body)
            .map_err(tantivy_err("reading body postings"))?;
        let path = segment
            .inverted_index(self.fields.path)
            .map_err(tantivy_err("reading path postings"))?;
        let mut matched = Vec::new();
        for term in terms {
            let in_body = contains(
                &body,
                &Term::from_field_text(self.fields.body, term),
                addr.doc_id,
            )?;
            if in_body
                || contains(
                    &path,
                    &Term::from_field_text(self.fields.path, term),
                    addr.doc_id,
                )?
            {
                matched.push(term.clone());
            }
        }
        Ok(matched)
    }
}

/// Whether `doc` is in the posting list of `term`.
fn contains(index: &InvertedIndexReader, term: &Term, doc: DocId) -> Result<bool, LexicalError> {
    let postings = index
        .read_postings(term, IndexRecordOption::Basic)
        .map_err(|source| LexicalError::Io {
            context: "reading postings",
            source,
        })?;
    let Some(mut postings) = postings else {
        return Ok(false);
    };
    // `seek` requires a target at or after the current document.
    if postings.doc() > doc {
        return Ok(false);
    }
    Ok(postings.seek(doc) == doc)
}

/// Single-threaded writer for a [`LexicalIndex`].
///
/// Changes become visible to searches only after [`LexicalWriter::commit`].
/// Dropping the writer without committing discards uncommitted changes.
pub struct LexicalWriter {
    writer: IndexWriter<TantivyDocument>,
    reader: IndexReader,
    fields: Fields,
}

impl LexicalWriter {
    /// Adds a document. It does **not** replace an existing document with the
    /// same `id`; call [`delete`](Self::delete) first to update.
    ///
    /// # Errors
    /// Fails if Tantivy rejects the document.
    pub fn add(&mut self, doc: LexicalDoc<'_>) -> Result<(), LexicalError> {
        let mut out = TantivyDocument::default();
        out.add_text(self.fields.id, doc.id);
        out.add_text(self.fields.path_text, doc.path);
        out.add_pre_tokenized_text(self.fields.path, pre_tokenized(&tokenize(doc.path)));
        out.add_pre_tokenized_text(self.fields.body, pre_tokenized(&tokenize(doc.text)));
        self.writer
            .add_document(out)
            .map_err(tantivy_err("adding document"))?;
        Ok(())
    }

    /// Deletes every document with this `id` (including ones added earlier
    /// in the same, not yet committed, batch). Unknown ids are ignored.
    ///
    /// # Errors
    /// Currently infallible; the `Result` leaves room for future backends.
    pub fn delete(&mut self, id: &str) -> Result<(), LexicalError> {
        self.writer
            .delete_term(Term::from_field_text(self.fields.id, id));
        Ok(())
    }

    /// Commits all pending changes and reloads the index reader so the next
    /// search sees them.
    ///
    /// # Errors
    /// Fails if the commit or the reader reload fails.
    pub fn commit(mut self) -> Result<(), LexicalError> {
        self.writer.commit().map_err(tantivy_err("committing"))?;
        self.reader
            .reload()
            .map_err(tantivy_err("reloading reader"))?;
        Ok(())
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU32, Ordering};

    const CORPUS: &[(&str, &str, &str)] = &[
        (
            "billing-api/src/subscription.service.ts",
            "billing-api/src/subscription.service.ts",
            "export class SubscriptionService { async cancelSubscription(id: string) { return this.repo.cancel(id); } }",
        ),
        (
            "billing-api/src/invoice.service.ts",
            "billing-api/src/invoice.service.ts",
            "Creates invoices. A subscription is billed monthly. See the invoice model.",
        ),
        (
            "docs/odeme.md",
            "docs/odeme.md",
            "Ödeme akışı kart bilgisi ile başlar.",
        ),
        (
            "web/src/config_loader.rs",
            "web/src/config_loader.rs",
            "pub fn load_config() { parse_v2_config(); }",
        ),
        ("web/README.md", "web/README.md", "Nothing to see here."),
    ];

    fn build(docs: &[(&str, &str, &str)]) -> LexicalIndex {
        let idx = LexicalIndex::create_in_ram().unwrap();
        let mut w = idx.writer().unwrap();
        for (id, path, text) in docs {
            w.add(LexicalDoc { id, path, text }).unwrap();
        }
        w.commit().unwrap();
        idx
    }

    fn ids(hits: &[LexicalHit]) -> Vec<&str> {
        hits.iter().map(|h| h.id.as_str()).collect()
    }

    struct TempDir(PathBuf);
    impl TempDir {
        fn new() -> Self {
            static N: AtomicU32 = AtomicU32::new(0);
            let p = std::env::temp_dir().join(format!(
                "knowell-lexical-test-{}-{}",
                std::process::id(),
                N.fetch_add(1, Ordering::SeqCst)
            ));
            let _ = std::fs::remove_dir_all(&p);
            Self(p)
        }
    }
    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn reports_matched_terms_and_coordination_rewards_coverage() {
        let index = build(&[
            (
                "a",
                "src/subscription.ts",
                "export function cancelSubscription() {}",
            ),
            ("b", "src/rare.ts", "zyxcancel zyxcancel zyxcancel cancel"),
        ]);
        let hits = index.search("cancel subscription", 10).unwrap();
        let first = hits.first().unwrap();
        assert_eq!(first.id, "a");
        assert_eq!(first.matched_terms, ["cancel", "subscription"]);
        let b = hits.iter().find(|h| h.id == "b").unwrap();
        assert_eq!(b.matched_terms, ["cancel"]);

        let plain = index
            .search_with(
                "cancel subscription",
                10,
                &SearchOptions { coordination: 0.0 },
            )
            .unwrap();
        let coord_b = b.score;
        let plain_b = plain.iter().find(|h| h.id == "b").unwrap().score;
        assert!(coord_b < plain_b, "half coverage halves the score");
    }

    #[test]
    fn ranking_prefers_dense_match() {
        let idx = build(CORPUS);
        let hits = idx.search("cancel subscription", 10).unwrap();
        assert_eq!(
            hits.first().unwrap().id,
            "billing-api/src/subscription.service.ts"
        );
        assert!(ids(&hits).contains(&"billing-api/src/invoice.service.ts"));
        assert!(hits[0].score > hits[1].score);
    }

    #[test]
    fn camel_and_snake_queries_match() {
        let idx = build(CORPUS);
        for q in [
            "cancelSubscription",
            "cancel_subscription",
            "CANCEL SUBSCRIPTION",
        ] {
            let hits = idx.search(q, 3).unwrap();
            assert_eq!(
                hits.first().map(|h| h.id.as_str()),
                Some("billing-api/src/subscription.service.ts"),
                "query {q}"
            );
        }
        let hits = idx.search("parse_v2_config", 3).unwrap();
        assert_eq!(hits[0].id, "web/src/config_loader.rs");
        let hits = idx.search("parseV2Config", 3).unwrap();
        assert_eq!(hits[0].id, "web/src/config_loader.rs");
        let hits = idx.search("v2", 3).unwrap();
        assert_eq!(hits[0].id, "web/src/config_loader.rs");
    }

    #[test]
    fn turkish_folding_matches() {
        let idx = build(CORPUS);
        for q in ["ödeme", "odeme", "ÖDEME", "kart bilgisi"] {
            let hits = idx.search(q, 3).unwrap();
            assert_eq!(
                hits.first().map(|h| h.id.as_str()),
                Some("docs/odeme.md"),
                "query {q}"
            );
        }
    }

    #[test]
    fn path_only_match() {
        let idx = build(CORPUS);
        // "readme" appears only in the path.
        let hits = idx.search("readme", 5).unwrap();
        assert_eq!(ids(&hits), ["web/README.md"]);
        assert_eq!(hits[0].path, "web/README.md");
    }

    #[test]
    fn path_boost_applies() {
        let idx = build(&[
            ("a", "src/alpha.rs", "nothing relevant"),
            ("b", "src/other.rs", "alpha"),
        ]);
        let hits = idx.search("alpha", 5).unwrap();
        assert_eq!(ids(&hits), ["a", "b"]);
    }

    #[test]
    fn deletion_and_update() {
        let idx = build(CORPUS);
        assert_eq!(idx.num_docs(), 5);
        let mut w = idx.writer().unwrap();
        w.delete("docs/odeme.md").unwrap();
        w.delete("does-not-exist").unwrap();
        w.commit().unwrap();
        assert_eq!(idx.num_docs(), 4);
        assert!(idx.search("odeme", 5).unwrap().is_empty());

        let mut w = idx.writer().unwrap();
        w.delete("web/README.md").unwrap();
        w.add(LexicalDoc {
            id: "web/README.md",
            path: "web/README.md",
            text: "fresh zebra content",
        })
        .unwrap();
        w.commit().unwrap();
        assert_eq!(idx.num_docs(), 4);
        assert_eq!(ids(&idx.search("zebra", 5).unwrap()), ["web/README.md"]);
        assert!(idx.search("nothing", 5).unwrap().is_empty());
    }

    #[test]
    fn uncommitted_changes_are_invisible() {
        let idx = build(CORPUS);
        let mut w = idx.writer().unwrap();
        w.add(LexicalDoc {
            id: "x",
            path: "x.rs",
            text: "zebra",
        })
        .unwrap();
        assert!(idx.search("zebra", 5).unwrap().is_empty());
        w.commit().unwrap();
        assert_eq!(idx.search("zebra", 5).unwrap().len(), 1);
    }

    #[test]
    fn reopen_from_dir() {
        let tmp = TempDir::new();
        {
            let idx = LexicalIndex::create_in_dir(&tmp.0).unwrap();
            let mut w = idx.writer().unwrap();
            for (id, path, text) in CORPUS {
                w.add(LexicalDoc { id, path, text }).unwrap();
            }
            w.commit().unwrap();
            assert_eq!(idx.num_docs(), 5);
        }
        let idx = LexicalIndex::open_in_dir(&tmp.0).unwrap();
        assert_eq!(idx.num_docs(), 5);
        let hits = idx.search("cancel subscription", 3).unwrap();
        assert_eq!(hits[0].id, "billing-api/src/subscription.service.ts");
        assert!(LexicalIndex::create_in_dir(&tmp.0).is_err());
    }

    #[test]
    fn open_missing_dir_is_error() {
        let tmp = TempDir::new();
        assert!(LexicalIndex::open_in_dir(&tmp.0).is_err());
    }

    #[test]
    fn deterministic_across_builds() {
        let a = build(CORPUS)
            .search("subscription invoice service", 10)
            .unwrap();
        let b = build(CORPUS)
            .search("subscription invoice service", 10)
            .unwrap();
        assert!(!a.is_empty());
        assert_eq!(a, b);
    }

    #[test]
    fn ties_broken_by_id_and_limit_cut_is_stable() {
        let docs: Vec<(String, String, String)> = (0..40)
            .rev()
            .map(|i| {
                (
                    format!("id{i:02}"),
                    format!("p{i:02}.txt"),
                    "tiedterm".to_owned(),
                )
            })
            .collect();
        let refs: Vec<(&str, &str, &str)> = docs
            .iter()
            .map(|(a, b, c)| (a.as_str(), b.as_str(), c.as_str()))
            .collect();
        let idx = build(&refs);
        let hits = idx.search("tiedterm", 5).unwrap();
        assert_eq!(ids(&hits), ["id00", "id01", "id02", "id03", "id04"]);
        let all = idx.search("tiedterm", 100).unwrap();
        assert_eq!(all.len(), 40);
    }

    #[test]
    fn special_characters_and_empty_queries() {
        let idx = build(CORPUS);
        for q in [
            "",
            "   ",
            "path:foo AND (bar OR \"baz\") -qux ^2 [a TO b] *?",
            "\"unterminated",
            "((((",
            "🚀🔥",
            "a",
            "\u{0}\u{1}",
        ] {
            let r = idx.search(q, 5);
            assert!(r.is_ok(), "query {q:?}");
        }
        assert!(idx.search("", 5).unwrap().is_empty());
        assert!(idx.search("cancel", 0).unwrap().is_empty());
        let hits = idx.search("title:\"cancel\" (subscription)", 3).unwrap();
        assert_eq!(hits[0].id, "billing-api/src/subscription.service.ts");
    }

    #[test]
    fn empty_index_search() {
        let idx = LexicalIndex::create_in_ram().unwrap();
        assert!(idx.search("anything", 5).unwrap().is_empty());
        assert_eq!(idx.num_docs(), 0);
    }

    #[test]
    fn second_writer_is_rejected_while_first_lives() {
        let tmp = TempDir::new();
        let idx = LexicalIndex::create_in_dir(&tmp.0).unwrap();
        let _w = idx.writer().unwrap();
        assert!(idx.writer().is_err());
    }
}
