//! The retrieval interface and the grep baseline.

use crate::corpus::Corpus;
use crate::error::EvalError;

/// One ranked result.
#[derive(Debug, Clone, PartialEq)]
pub struct RankedDoc {
    /// Document id (`<project>/<path>`).
    pub id: String,
    /// Retriever-specific score; higher is better. Only the order matters to
    /// the metrics.
    pub score: f64,
}

/// Anything that ranks corpus documents for a query.
///
/// Contract: return at most `k` documents, best first, without duplicate
/// ids. Returning nothing is a valid answer ("abstain") and is what an
/// `absent` query rewards.
pub trait Retriever {
    /// Stable name used as the report row (e.g. `grep`, `bm25`, `hybrid`).
    fn name(&self) -> &str;

    /// Ranks documents for `query`.
    fn search(&self, query: &str, k: usize) -> Result<Vec<RankedDoc>, EvalError>;
}

/// English stopwords and question words dropped from grep queries. They are
/// words an agent would not type into `grep`.
pub const STOPWORDS_EN: &[&str] = &[
    "about",
    "after",
    "all",
    "also",
    "and",
    "any",
    "are",
    "back",
    "been",
    "before",
    "but",
    "can",
    "could",
    "did",
    "does",
    "doing",
    "done",
    "each",
    "for",
    "from",
    "get",
    "gets",
    "got",
    "had",
    "has",
    "have",
    "how",
    "into",
    "its",
    "just",
    "more",
    "most",
    "not",
    "now",
    "off",
    "once",
    "only",
    "other",
    "our",
    "out",
    "over",
    "should",
    "some",
    "such",
    "than",
    "that",
    "the",
    "their",
    "them",
    "then",
    "there",
    "these",
    "they",
    "this",
    "those",
    "through",
    "too",
    "under",
    "until",
    "use",
    "used",
    "very",
    "was",
    "were",
    "what",
    "when",
    "where",
    "which",
    "while",
    "who",
    "whom",
    "why",
    "will",
    "with",
    "would",
    "you",
    "your",
    "code",
    "file",
    "files",
    "find",
    "function",
    "implemented",
    "defined",
    "happens",
    "happen",
    "someone",
    "something",
];

/// Turkish stopwords and question words dropped from grep queries.
pub const STOPWORDS_TR: &[&str] = &[
    "acaba",
    "ama",
    "ancak",
    "bir",
    "biz",
    "bize",
    "bizim",
    "bu",
    "buna",
    "bunu",
    "çok",
    "daha",
    "diye",
    "gibi",
    "göre",
    "hangi",
    "hangisi",
    "hem",
    "her",
    "hiç",
    "için",
    "ile",
    "ise",
    "kadar",
    "kim",
    "kimler",
    "mı",
    "mi",
    "mu",
    "mü",
    "nasıl",
    "ne",
    "neden",
    "nedir",
    "nerede",
    "nereden",
    "nereye",
    "niçin",
    "niye",
    "olan",
    "olarak",
    "oluyor",
    "onu",
    "sonra",
    "şey",
    "şu",
    "var",
    "veya",
    "yani",
    "yok",
    "kod",
    "kodu",
    "kodda",
    "dosya",
    "dosyada",
    "dosyası",
    "fonksiyon",
    "yapılıyor",
    "ediliyor",
    "ediyor",
];

/// Minimum length (in characters) of a query word.
pub const MIN_WORD_CHARS: usize = 3;

/// Approximates what a coding agent does with `grep` today.
///
/// Query processing:
/// 1. lowercase the query (Unicode lowercase; Turkish `I`/`İ` follow the
///    default Unicode mapping);
/// 2. split into words: maximal runs of alphanumeric characters or `_`
///    (so `SMTP_HOST` stays one word, `SubscriptionService.cancelSubscription`
///    becomes two, `subscription.cancelled` becomes two);
/// 3. drop words shorter than [`MIN_WORD_CHARS`] characters and words in
///    [`STOPWORDS_EN`] or [`STOPWORDS_TR`]; keep the first occurrence of each
///    remaining word.
///
/// Matching: for each document, the haystack is the lowercased
/// `id + "\n" + text`; a word matches when it occurs as a substring (no word
/// boundaries, like `grep -i`). For every word the non-overlapping
/// occurrences are counted.
///
/// Ranking: by the number of distinct matching words (descending), then the
/// total number of occurrences (descending), then the id (ascending).
/// Documents without any match are not returned, so a query whose words all
/// miss returns nothing. The score is `distinct + total / (total + 1)`,
/// which orders exactly like the two keys.
///
/// Cost is linear in corpus size per query term; the lowercased haystacks
/// are built once in [`GrepRetriever::new`].
#[derive(Debug, Clone)]
pub struct GrepRetriever {
    /// `(id, lowercased "id\ntext")`, in corpus order.
    haystacks: Vec<(String, String)>,
}

impl GrepRetriever {
    /// Report name of this retriever.
    pub const NAME: &'static str = "grep";

    /// Prepares the lowercased haystacks of `corpus`.
    pub fn new(corpus: &Corpus) -> Self {
        let haystacks = corpus
            .docs()
            .iter()
            .map(|doc| {
                let mut hay = String::with_capacity(doc.id.len() + 1 + doc.text.len());
                hay.push_str(&doc.id);
                hay.push('\n');
                hay.push_str(&doc.text);
                (doc.id.clone(), hay.to_lowercase())
            })
            .collect();
        Self { haystacks }
    }

    /// The words that would be searched for `query` (see the type docs).
    pub fn query_words(query: &str) -> Vec<String> {
        let lower = query.to_lowercase();
        let mut words: Vec<String> = Vec::new();
        for word in lower.split(|c: char| !(c.is_alphanumeric() || c == '_')) {
            if word.chars().count() < MIN_WORD_CHARS
                || STOPWORDS_EN.contains(&word)
                || STOPWORDS_TR.contains(&word)
                || words.iter().any(|w| w == word)
            {
                continue;
            }
            words.push(word.to_owned());
        }
        words
    }
}

impl Retriever for GrepRetriever {
    fn name(&self) -> &str {
        Self::NAME
    }

    fn search(&self, query: &str, k: usize) -> Result<Vec<RankedDoc>, EvalError> {
        let words = Self::query_words(query);
        if words.is_empty() || k == 0 {
            return Ok(Vec::new());
        }
        let mut hits: Vec<(usize, usize, &str)> = Vec::new();
        for (id, hay) in &self.haystacks {
            let mut distinct = 0usize;
            let mut total = 0usize;
            for word in &words {
                let count = hay.matches(word.as_str()).count();
                if count > 0 {
                    distinct += 1;
                    total += count;
                }
            }
            if distinct > 0 {
                hits.push((distinct, total, id.as_str()));
            }
        }
        hits.sort_by(|a, b| {
            b.0.cmp(&a.0)
                .then_with(|| b.1.cmp(&a.1))
                .then_with(|| a.2.cmp(b.2))
        });
        Ok(hits
            .into_iter()
            .take(k)
            .map(|(distinct, total, id)| RankedDoc {
                id: id.to_owned(),
                score: distinct as f64 + total as f64 / (total as f64 + 1.0),
            })
            .collect())
    }
}

/// Orders ranked documents by descending score, then ascending id; useful
/// for retrievers that produce unordered scores.
pub fn sort_ranked(docs: &mut [RankedDoc]) {
    docs.sort_by(|a, b| b.score.total_cmp(&a.score).then_with(|| a.id.cmp(&b.id)));
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::corpus::CorpusDoc;

    fn corpus(docs: &[(&str, &str)]) -> Corpus {
        Corpus::from_docs(docs.iter().map(|(id, text)| CorpusDoc {
            id: (*id).to_owned(),
            text: (*text).to_owned(),
        }))
        .unwrap()
    }

    fn ids(results: &[RankedDoc]) -> Vec<&str> {
        results.iter().map(|r| r.id.as_str()).collect()
    }

    #[test]
    fn query_words_drop_stopwords_short_words_and_duplicates() {
        assert_eq!(
            GrepRetriever::query_words("Where is the SMTP_HOST configured? SMTP_host!"),
            ["smtp_host", "configured"]
        );
        assert_eq!(
            GrepRetriever::query_words("SubscriptionService.cancelSubscription"),
            ["subscriptionservice", "cancelsubscription"]
        );
        assert_eq!(
            GrepRetriever::query_words("müşteriden iki kez ücret alınmasını nerede engelliyoruz?"),
            [
                "müşteriden",
                "iki",
                "kez",
                "ücret",
                "alınmasını",
                "engelliyoruz"
            ]
        );
        assert!(GrepRetriever::query_words("is it on? ne var").is_empty());
    }

    #[test]
    fn ranks_by_distinct_then_total_then_id() {
        let c = corpus(&[
            ("p/c.txt", "alpha alpha alpha"),
            ("p/b.txt", "alpha beta"),
            ("p/a.txt", "beta alpha"),
            ("p/d.txt", "nothing here"),
            ("p/e.txt", "ALPHA beta beta"),
        ]);
        let grep = GrepRetriever::new(&c);
        let results = grep.search("alpha beta", 10).unwrap();
        assert_eq!(ids(&results), ["p/e.txt", "p/a.txt", "p/b.txt", "p/c.txt"]);
        assert!(results.windows(2).all(|w| w[0].score >= w[1].score));
        assert_eq!(
            ids(&grep.search("alpha beta", 2).unwrap()),
            ["p/e.txt", "p/a.txt"]
        );
    }

    #[test]
    fn matches_substrings_and_paths_case_insensitively() {
        let c = corpus(&[
            (
                "svc/src/billing.ts",
                "export function cancelSubscription() {}",
            ),
            ("svc/src/ledger.ts", "nothing"),
        ]);
        let grep = GrepRetriever::new(&c);
        assert_eq!(
            ids(&grep.search("CANCEL", 5).unwrap()),
            ["svc/src/billing.ts"]
        );
        assert_eq!(
            ids(&grep.search("billing", 5).unwrap()),
            ["svc/src/billing.ts"]
        );
        assert_eq!(
            ids(&grep.search("LEDGER", 5).unwrap()),
            ["svc/src/ledger.ts"]
        );
    }

    #[test]
    fn abstains_when_nothing_matches() {
        let c = corpus(&[("p/a.txt", "alpha")]);
        let grep = GrepRetriever::new(&c);
        assert!(grep.search("zeta", 10).unwrap().is_empty());
        assert!(grep.search("the and", 10).unwrap().is_empty());
        assert!(grep.search("alpha", 0).unwrap().is_empty());
        assert_eq!(grep.name(), "grep");
    }

    #[test]
    fn sort_ranked_orders_by_score_then_id() {
        let mut docs = vec![
            RankedDoc {
                id: "b".into(),
                score: 1.0,
            },
            RankedDoc {
                id: "a".into(),
                score: 1.0,
            },
            RankedDoc {
                id: "c".into(),
                score: 2.0,
            },
        ];
        sort_ranked(&mut docs);
        assert_eq!(ids(&docs), ["c", "a", "b"]);
    }
}
