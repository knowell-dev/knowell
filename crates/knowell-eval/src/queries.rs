//! Graded query sets.
//!
//! A query set is a TOML file (`queries/<fixture>.toml`) with one
//! `[[query]]` table per query. Every query names the documents that answer
//! it with a graded relevance (1–3); `absent` queries ask for behaviour that
//! does not exist and list nothing. The built-in set for the
//! [`FIXTURE_NAME`](crate::FIXTURE_NAME) fixture is embedded in the
//! binary.

use std::collections::{BTreeMap, BTreeSet};

use knowell_core::{ContentHash, Name, RepoPath};
use serde::{Deserialize, Serialize};

use crate::error::EvalError;
use crate::fixture::{FileRole, Fixture};

/// The built-in query set for the `acme-goods` fixture.
pub const BUILTIN_QUERIES_TOML: &str = include_str!("../queries/acme-goods.toml");

/// Highest relevance grade.
pub const MAX_GRADE: u8 = 3;

/// Query language.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Lang {
    /// English.
    En,
    /// Turkish.
    Tr,
}

impl Lang {
    /// Code as written in query files and reports.
    pub fn as_str(self) -> &'static str {
        match self {
            Lang::En => "en",
            Lang::Tr => "tr",
        }
    }
}

/// What a query tests.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum QueryKind {
    /// An exact identifier.
    Symbol,
    /// Behaviour described in other words than the code uses.
    Behavior,
    /// A flow spanning several projects.
    CrossProject,
    /// Producers and consumers of an endpoint, event, RPC or table.
    Contract,
    /// Environment variables, compose and deployment settings.
    Config,
    /// Translations and locale handling.
    I18n,
    /// Decisions and their history (ADRs, runbooks).
    HistoryDoc,
    /// Behaviour that does not exist; the right answer is nothing.
    Absent,
}

impl QueryKind {
    /// Name as written in query files and reports.
    pub fn as_str(self) -> &'static str {
        match self {
            QueryKind::Symbol => "symbol",
            QueryKind::Behavior => "behavior",
            QueryKind::CrossProject => "cross_project",
            QueryKind::Contract => "contract",
            QueryKind::Config => "config",
            QueryKind::I18n => "i18n",
            QueryKind::HistoryDoc => "history_doc",
            QueryKind::Absent => "absent",
        }
    }
}

/// One relevance judgment.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Judgment {
    /// Document id `<project>/<path>`.
    pub doc: String,
    /// 1 = useful context, 2 = directly involved, 3 = the answer.
    pub grade: u8,
}

/// One query with its judgments.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Query {
    /// Unique slug.
    pub id: String,
    /// Query language.
    pub lang: Lang,
    /// What the query tests.
    pub kind: QueryKind,
    /// The question, as a user or agent would type it.
    pub text: String,
    /// Relevant documents (empty exactly for `absent` queries).
    #[serde(default)]
    pub relevant: Vec<Judgment>,
    /// Free-text explanation (distractors, planted issues, paraphrases).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub notes: Option<String>,
}

impl Query {
    /// The grade of `doc`, or 0 when it is not judged relevant.
    pub fn grade_of(&self, doc: &str) -> u8 {
        self.relevant
            .iter()
            .find(|j| j.doc == doc)
            .map_or(0, |j| j.grade)
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct QueryFile {
    version: u32,
    fixture: String,
    #[serde(default)]
    query: Vec<Query>,
}

/// A validated query set.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct QuerySet {
    /// Name of the fixture the judgments refer to.
    pub fixture: String,
    /// Queries in file order.
    pub queries: Vec<Query>,
}

/// Identity of a query set, carried into reports.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct QuerySetInfo {
    /// Fixture the set belongs to.
    pub fixture: String,
    /// [`QuerySet::hash`].
    pub hash: ContentHash,
    /// Number of queries.
    pub queries: usize,
    /// Queries scored with ranking metrics (all but `absent`).
    pub ranked: usize,
    /// `absent` queries.
    pub absent: usize,
}

impl QuerySet {
    /// Parses and validates the built-in set, including that every judged
    /// document is a core file of the fixture.
    pub fn builtin() -> Result<QuerySet, EvalError> {
        let set = QuerySet::from_toml_str(BUILTIN_QUERIES_TOML)?;
        let core: BTreeSet<String> = crate::fixture::core_doc_ids().into_iter().collect();
        for query in &set.queries {
            for judgment in &query.relevant {
                if !core.contains(&judgment.doc) {
                    return Err(invalid(
                        &query.id,
                        format!("`{}` is not a core file of the fixture", judgment.doc),
                    ));
                }
            }
        }
        Ok(set)
    }

    /// Parses a query-set file and checks its structure: version 1, unique
    /// slug ids, non-empty text, grades 1–3, well-formed and unique doc ids,
    /// and `relevant` empty exactly for `absent` queries. CRLF line endings
    /// are normalised first so the hash is platform-independent.
    pub fn from_toml_str(text: &str) -> Result<QuerySet, EvalError> {
        let text = text.replace("\r\n", "\n");
        let file: QueryFile =
            toml::from_str(&text).map_err(|e| EvalError::QuerySet(e.to_string()))?;
        if file.version != 1 {
            return Err(EvalError::QuerySet(format!(
                "unsupported version {}; expected 1",
                file.version
            )));
        }
        if Name::new(file.fixture.as_str()).is_err() {
            return Err(EvalError::QuerySet(format!(
                "fixture `{}` is not a valid name",
                file.fixture
            )));
        }
        if file.query.is_empty() {
            return Err(EvalError::QuerySet(
                "the set contains no queries".to_owned(),
            ));
        }
        let mut ids = BTreeSet::new();
        for (index, query) in file.query.iter().enumerate() {
            if Name::new(query.id.as_str()).is_err() {
                return Err(invalid(
                    &format!("#{}", index + 1),
                    format!("id `{}` must be a lowercase slug", query.id),
                ));
            }
            if !ids.insert(query.id.as_str()) {
                return Err(invalid(&query.id, "duplicate id"));
            }
            validate_query(query)?;
        }
        Ok(QuerySet {
            fixture: file.fixture,
            queries: file.query,
        })
    }

    /// Checks every judgment against a generated fixture: the document must
    /// exist and must be a core file (noise is never relevant).
    pub fn validate_against(&self, fixture: &Fixture) -> Result<(), EvalError> {
        if self.fixture != fixture.name() {
            return Err(EvalError::QuerySet(format!(
                "query set is for fixture `{}`, not `{}`",
                self.fixture,
                fixture.name()
            )));
        }
        for query in &self.queries {
            for judgment in &query.relevant {
                match fixture.file(&judgment.doc) {
                    None => {
                        return Err(invalid(
                            &query.id,
                            format!("`{}` does not exist in the fixture", judgment.doc),
                        ));
                    }
                    Some(file) if file.role != FileRole::Core => {
                        return Err(invalid(
                            &query.id,
                            format!("`{}` is generated noise", judgment.doc),
                        ));
                    }
                    Some(_) => {}
                }
            }
        }
        Ok(())
    }

    /// BLAKE3 of the canonical JSON form (independent of comments and
    /// formatting of the source file).
    pub fn hash(&self) -> ContentHash {
        // Serialising plain strings, enums and integers cannot fail; an
        // empty body would still yield a stable (if useless) hash.
        let json = serde_json::to_vec(self).unwrap_or_default();
        ContentHash::of_parts([b"knowell-eval/queries/v1".as_slice(), json.as_slice()])
    }

    /// Identity for reports.
    pub fn info(&self) -> QuerySetInfo {
        let absent = self
            .queries
            .iter()
            .filter(|q| q.kind == QueryKind::Absent)
            .count();
        QuerySetInfo {
            fixture: self.fixture.clone(),
            hash: self.hash(),
            queries: self.queries.len(),
            ranked: self.queries.len() - absent,
            absent,
        }
    }

    /// Number of queries per kind.
    pub fn count_by_kind(&self) -> BTreeMap<&'static str, usize> {
        let mut counts = BTreeMap::new();
        for query in &self.queries {
            *counts.entry(query.kind.as_str()).or_insert(0) += 1;
        }
        counts
    }

    /// Number of queries per language.
    pub fn count_by_lang(&self) -> BTreeMap<&'static str, usize> {
        let mut counts = BTreeMap::new();
        for query in &self.queries {
            *counts.entry(query.lang.as_str()).or_insert(0) += 1;
        }
        counts
    }
}

fn validate_query(query: &Query) -> Result<(), EvalError> {
    if query.text.trim().is_empty() {
        return Err(invalid(&query.id, "text must not be empty"));
    }
    match (query.kind == QueryKind::Absent, query.relevant.is_empty()) {
        (true, false) => {
            return Err(invalid(
                &query.id,
                "absent queries must not list relevant documents",
            ));
        }
        (false, true) => return Err(invalid(&query.id, "list at least one relevant document")),
        _ => {}
    }
    let mut docs = BTreeSet::new();
    for judgment in &query.relevant {
        if !(1..=MAX_GRADE).contains(&judgment.grade) {
            return Err(invalid(
                &query.id,
                format!(
                    "grade {} of `{}` must be 1, 2 or 3",
                    judgment.grade, judgment.doc
                ),
            ));
        }
        if !valid_doc_id(&judgment.doc) {
            return Err(invalid(
                &query.id,
                format!("`{}` is not a `<project>/<path>` document id", judgment.doc),
            ));
        }
        if !docs.insert(judgment.doc.as_str()) {
            return Err(invalid(
                &query.id,
                format!("`{}` is listed twice", judgment.doc),
            ));
        }
    }
    Ok(())
}

fn valid_doc_id(doc: &str) -> bool {
    doc.split_once('/')
        .is_some_and(|(project, path)| Name::new(project).is_ok() && RepoPath::new(path).is_ok())
}

fn invalid(query: &str, reason: impl Into<String>) -> EvalError {
    EvalError::InvalidQuery {
        query: query.to_owned(),
        reason: reason.into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fixture::{FixtureSpec, Scale, generate};

    const MINIMAL: &str = r#"
version = 1
fixture = "acme-goods"

[[query]]
id = "q1"
lang = "en"
kind = "symbol"
text = "IdempotencyStore"
relevant = [{ doc = "ledger-service/ledger/payments/idempotency.py", grade = 3 }]

[[query]]
id = "q2"
lang = "tr"
kind = "absent"
text = "yok"
"#;

    fn err_of(text: &str) -> String {
        QuerySet::from_toml_str(text).unwrap_err().to_string()
    }

    #[test]
    fn builtin_set_meets_the_coverage_rules() {
        let set = QuerySet::builtin().unwrap();
        let total = set.queries.len();
        assert!(total >= 48, "{total} queries");
        let by_lang = set.count_by_lang();
        let tr = by_lang.get("tr").copied().unwrap_or(0);
        assert!(
            tr * 10 >= total * 3,
            "only {tr} of {total} queries are Turkish"
        );
        let by_kind = set.count_by_kind();
        assert!(by_kind.get("absent").copied().unwrap_or(0) >= 5);
        for kind in [
            "symbol",
            "behavior",
            "cross_project",
            "contract",
            "config",
            "i18n",
            "history_doc",
        ] {
            assert!(by_kind.get(kind).copied().unwrap_or(0) >= 3, "{kind}");
        }
        let paraphrases = set
            .queries
            .iter()
            .filter(|q| q.notes.as_deref().is_some_and(|n| n.contains("araphrase")))
            .count();
        assert!(paraphrases >= 5);
        assert_eq!(set.info().ranked + set.info().absent, total);
    }

    #[test]
    fn builtin_judgments_are_core_files_at_every_scale() {
        let set = QuerySet::builtin().unwrap();
        for scale in [Scale::Small, Scale::Medium] {
            for seed in [0, 42, u64::MAX] {
                let fixture = generate(&FixtureSpec { seed, scale });
                set.validate_against(&fixture).unwrap();
            }
        }
    }

    #[test]
    fn env_file_is_never_relevant() {
        let set = QuerySet::builtin().unwrap();
        assert!(set.queries.iter().all(|q| q.grade_of("infra/.env") == 0));
    }

    #[test]
    fn parses_minimal_set() {
        let set = QuerySet::from_toml_str(MINIMAL).unwrap();
        assert_eq!(set.queries.len(), 2);
        assert_eq!(
            set.queries[0].grade_of("ledger-service/ledger/payments/idempotency.py"),
            3
        );
        assert_eq!(set.queries[0].grade_of("other"), 0);
        assert_eq!(set.queries[1].kind, QueryKind::Absent);
    }

    #[test]
    fn hash_ignores_formatting_and_line_endings() {
        let a = QuerySet::from_toml_str(MINIMAL).unwrap();
        let b = QuerySet::from_toml_str(&MINIMAL.replace('\n', "\r\n")).unwrap();
        let c = QuerySet::from_toml_str(&format!("# comment\n{MINIMAL}")).unwrap();
        assert_eq!(a.hash(), b.hash());
        assert_eq!(a.hash(), c.hash());
        let d = QuerySet::from_toml_str(&MINIMAL.replace("grade = 3", "grade = 2")).unwrap();
        assert_ne!(a.hash(), d.hash());
    }

    #[test]
    fn rejects_structural_errors() {
        assert!(err_of(&MINIMAL.replace("version = 1", "version = 2")).contains("version"));
        assert!(err_of(&MINIMAL.replace("id = \"q2\"", "id = \"q1\"")).contains("duplicate"));
        assert!(err_of(&MINIMAL.replace("id = \"q2\"", "id = \"Q 2\"")).contains("slug"));
        assert!(err_of(&MINIMAL.replace("grade = 3", "grade = 4")).contains("grade"));
        assert!(err_of(&MINIMAL.replace("grade = 3", "grade = 0")).contains("grade"));
        assert!(err_of(&MINIMAL.replace("text = \"yok\"", "text = \"  \"")).contains("empty"));
        assert!(
            err_of(&MINIMAL.replace("kind = \"absent\"", "kind = \"symbol\""))
                .contains("at least one")
        );
        assert!(
            err_of(&MINIMAL.replace("kind = \"symbol\"", "kind = \"absent\""))
                .contains("must not list")
        );
        assert!(
            err_of(&MINIMAL.replace("kind = \"symbol\"", "kind = \"vibes\""))
                .contains("invalid query set")
        );
        assert!(
            err_of(&MINIMAL.replace("ledger-service/ledger", "../ledger")).contains("document id")
        );
        assert!(
            err_of(&MINIMAL.replace("lang = \"en\"", "lang = \"de\""))
                .contains("invalid query set")
        );
        assert!(
            err_of(&MINIMAL.replace("text = \"yok\"", "text = \"yok\"\nextra = 1"))
                .contains("invalid query set")
        );
        assert!(err_of("version = 1\nfixture = \"acme-goods\"\n").contains("no queries"));
        assert!(err_of("not toml at all [").contains("invalid query set"));
        let twice = MINIMAL.replace(
            "relevant = [{ doc = \"ledger-service/ledger/payments/idempotency.py\", grade = 3 }]",
            "relevant = [{ doc = \"a/b\", grade = 3 }, { doc = \"a/b\", grade = 1 }]",
        );
        assert!(err_of(&twice).contains("twice"));
    }

    #[test]
    fn validate_against_rejects_unknown_and_noise_docs() {
        let fixture = generate(&FixtureSpec {
            seed: 1,
            scale: Scale::Small,
        });
        let unknown = QuerySet::from_toml_str(
            &MINIMAL.replace("payments/idempotency.py", "payments/nope.py"),
        )
        .unwrap();
        assert!(
            unknown
                .validate_against(&fixture)
                .unwrap_err()
                .to_string()
                .contains("does not exist")
        );

        let noise = fixture
            .projects()
            .iter()
            .flat_map(|p| {
                p.files
                    .iter()
                    .filter(|f| f.role == FileRole::Noise)
                    .map(move |f| format!("{}/{}", p.name, f.path))
            })
            .next()
            .unwrap();
        let noisy = QuerySet::from_toml_str(
            &MINIMAL.replace("ledger-service/ledger/payments/idempotency.py", &noise),
        )
        .unwrap();
        assert!(
            noisy
                .validate_against(&fixture)
                .unwrap_err()
                .to_string()
                .contains("noise")
        );

        let other = QuerySet::from_toml_str(
            &MINIMAL.replace("fixture = \"acme-goods\"", "fixture = \"other\""),
        )
        .unwrap();
        assert!(other.validate_against(&fixture).is_err());
    }
}
