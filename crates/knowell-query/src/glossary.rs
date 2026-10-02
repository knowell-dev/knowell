use std::collections::BTreeMap;

use knowell_core::Name;
use serde::{Deserialize, Serialize};

use crate::QueryError;
use crate::error::echo;
use crate::text::fold;

/// Whether a glossary link was approved by a person.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TermStatus {
    /// Human-approved; expands queries by default.
    Approved,
    /// Proposed automatically (or by an agent); reported in the plan but only
    /// expands queries when [`PlanOptions::include_suggested`](crate::PlanOptions) is set.
    Suggested,
}

/// How the two sides of a glossary link relate.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TermRelation {
    /// Same meaning in the same language (`cancel` ↔ `terminate`).
    Synonym,
    /// Same meaning in another language (`ödeme` ↔ `payment`).
    Translation,
    /// Short form (`kdv` ↔ `vat`, `sub` ↔ `subscription`).
    Abbreviation,
    /// A business term and the name code uses for it (`üye` ↔ `member`).
    CodeName,
}

/// One glossary link: `term` expands to `expansion`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct GlossaryEntry {
    /// The query-side term (1-4 words).
    pub term: String,
    /// The term added to the search when `term` appears in a query.
    pub expansion: String,
    /// How the two relate.
    pub relation: TermRelation,
    /// Approval state.
    pub status: TermStatus,
    /// Business domain the link belongs to; `None` applies in every domain.
    #[serde(default)]
    pub domain: Option<Name>,
    /// Whether `expansion` also expands to `term`.
    #[serde(default)]
    pub bidirectional: bool,
}

impl GlossaryEntry {
    /// An approved, one-directional, domain-independent link.
    pub fn approved(
        term: impl Into<String>,
        expansion: impl Into<String>,
        relation: TermRelation,
    ) -> Self {
        Self {
            term: term.into(),
            expansion: expansion.into(),
            relation,
            status: TermStatus::Approved,
            domain: None,
            bidirectional: false,
        }
    }

    /// A suggested (not yet approved), one-directional, domain-independent link.
    pub fn suggested(
        term: impl Into<String>,
        expansion: impl Into<String>,
        relation: TermRelation,
    ) -> Self {
        Self {
            status: TermStatus::Suggested,
            ..Self::approved(term, expansion, relation)
        }
    }

    /// Makes the link apply in both directions.
    #[must_use]
    pub fn both_ways(mut self) -> Self {
        self.bidirectional = true;
        self
    }

    /// Restricts the link to one business domain.
    #[must_use]
    pub fn in_domain(mut self, domain: Name) -> Self {
        self.domain = Some(domain);
        self
    }
}

/// One glossary expansion found in a query.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Expansion {
    /// The (folded) query words that matched.
    pub matched: String,
    /// The glossary side that matched them.
    pub term: String,
    /// The term added to the search.
    pub expansion: String,
    /// How the two relate.
    pub relation: TermRelation,
    /// Approval state of the link.
    pub status: TermStatus,
    /// Domain of the link, if it is domain-specific.
    pub domain: Option<Name>,
    /// Whether the match relied on stripping an inflection suffix
    /// (`ödemeyi` → `ödeme`) rather than an exact word match.
    pub inflected: bool,
}

/// Domain glossary: query terms ↔ code names ↔ abbreviations ↔ translations.
///
/// Matching is on folded text (see the crate docs) over phrases of up to
/// [`Glossary::MAX_PHRASE_WORDS`] words, longest phrase first. A word may also
/// match a term it starts with when the remaining suffix is at most
/// [`Glossary::MAX_INFLECTION_SUFFIX`] characters and the term has at least
/// [`Glossary::MIN_INFLECTED_TERM`] characters — enough for Turkish case
/// suffixes (`aboneliği`, `ödemenin`) and English plurals. Expansion is one
/// hop: an expansion never expands again, so meaning cannot drift.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "Vec<GlossaryEntry>", into = "Vec<GlossaryEntry>")]
pub struct Glossary {
    entries: Vec<GlossaryEntry>,
    /// Folded key → (entry index, whether the key is the entry's expansion side).
    index: BTreeMap<String, Vec<(usize, bool)>>,
}

impl Glossary {
    /// Longest glossary term, in words.
    pub const MAX_PHRASE_WORDS: usize = 4;
    /// Minimum term length (characters) for suffix-tolerant matching.
    pub const MIN_INFLECTED_TERM: usize = 4;
    /// Longest suffix (characters) tolerated after a term.
    pub const MAX_INFLECTION_SUFFIX: usize = 6;
    /// Maximum length of either side of an entry, in characters.
    pub const MAX_TERM_LEN: usize = 200;

    /// Validates `entries` and builds the lookup index. Entries are sorted so
    /// that the glossary behaves the same whatever order they were loaded in;
    /// exact duplicates are removed.
    pub fn new(mut entries: Vec<GlossaryEntry>) -> Result<Self, QueryError> {
        for entry in &mut entries {
            entry.term = normalise_side(&entry.term, &entry.term)?;
            entry.expansion = normalise_side(&entry.expansion, &entry.term)?;
            if fold(&entry.term) == fold(&entry.expansion) {
                return Err(QueryError::InvalidGlossaryEntry {
                    term: echo(&entry.term),
                    reason: "term and expansion are the same",
                });
            }
        }
        entries.sort_by(|a, b| {
            (
                fold(&a.term),
                fold(&a.expansion),
                a.status,
                a.relation,
                &a.domain,
            )
                .cmp(&(
                    fold(&b.term),
                    fold(&b.expansion),
                    b.status,
                    b.relation,
                    &b.domain,
                ))
        });
        entries.dedup();
        let mut index: BTreeMap<String, Vec<(usize, bool)>> = BTreeMap::new();
        for (i, entry) in entries.iter().enumerate() {
            index.entry(fold(&entry.term)).or_default().push((i, false));
            if entry.bidirectional {
                index
                    .entry(fold(&entry.expansion))
                    .or_default()
                    .push((i, true));
            }
        }
        Ok(Self { entries, index })
    }

    /// The validated, sorted entries.
    pub fn entries(&self) -> &[GlossaryEntry] {
        &self.entries
    }

    /// Whether the glossary has no entries.
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Every expansion (approved and suggested) for a sequence of folded query
    /// words, in query order. `domain` selects domain-specific entries.
    pub(crate) fn expand(&self, words: &[String], domain: Option<&Name>) -> Vec<Expansion> {
        let mut out = Vec::new();
        let mut i = 0usize;
        while i < words.len() {
            let longest = Self::MAX_PHRASE_WORDS.min(words.len() - i);
            let mut consumed = 1usize;
            'phrase: for n in (1..=longest).rev() {
                let Some(window) = words.get(i..i + n) else {
                    continue;
                };
                let phrase = window.join(" ");
                for (key, inflected) in candidate_keys(&phrase) {
                    let found = self.entries_for(key, domain, &phrase, inflected);
                    if !found.is_empty() {
                        out.extend(found);
                        consumed = n;
                        break 'phrase;
                    }
                }
            }
            i += consumed;
        }
        out
    }

    fn entries_for(
        &self,
        key: &str,
        domain: Option<&Name>,
        phrase: &str,
        inflected: bool,
    ) -> Vec<Expansion> {
        let Some(hits) = self.index.get(key) else {
            return Vec::new();
        };
        hits.iter()
            .filter_map(|(i, reverse)| {
                let entry = self.entries.get(*i)?;
                let applies = entry.domain.as_ref().is_none_or(|d| Some(d) == domain);
                if !applies {
                    return None;
                }
                let (term, expansion) = if *reverse {
                    (&entry.expansion, &entry.term)
                } else {
                    (&entry.term, &entry.expansion)
                };
                Some(Expansion {
                    matched: phrase.to_owned(),
                    term: term.clone(),
                    expansion: expansion.clone(),
                    relation: entry.relation,
                    status: entry.status,
                    domain: entry.domain.clone(),
                    inflected,
                })
            })
            .collect()
    }
}

/// Keys to look up for a phrase, best first: the phrase itself, then the
/// phrase with an inflection suffix removed (longest stem first).
fn candidate_keys(phrase: &str) -> Vec<(&str, bool)> {
    let mut keys = vec![(phrase, false)];
    let boundaries: Vec<usize> = phrase.char_indices().map(|(i, _)| i).collect();
    let total = boundaries.len();
    for suffix_len in 1..=Glossary::MAX_INFLECTION_SUFFIX {
        let Some(stem_chars) = total.checked_sub(suffix_len) else {
            break;
        };
        if stem_chars < Glossary::MIN_INFLECTED_TERM {
            break;
        }
        let Some(cut) = boundaries.get(stem_chars) else {
            continue;
        };
        let (Some(stem), Some(suffix)) = (phrase.get(..*cut), phrase.get(*cut..)) else {
            continue;
        };
        if suffix.contains(' ') || stem.ends_with(' ') {
            break;
        }
        keys.push((stem, true));
    }
    keys
}

fn normalise_side(text: &str, term: &str) -> Result<String, QueryError> {
    let invalid = |reason| QueryError::InvalidGlossaryEntry {
        term: echo(term),
        reason,
    };
    let collapsed = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if collapsed.is_empty() {
        return Err(invalid("term and expansion must not be empty"));
    }
    if collapsed.chars().count() > Glossary::MAX_TERM_LEN {
        return Err(invalid("term and expansion must be at most 200 characters"));
    }
    if collapsed.chars().any(char::is_control) {
        return Err(invalid(
            "term and expansion must not contain control characters",
        ));
    }
    if collapsed.split(' ').count() > Glossary::MAX_PHRASE_WORDS {
        return Err(invalid("terms are at most 4 words long"));
    }
    Ok(collapsed)
}

impl TryFrom<Vec<GlossaryEntry>> for Glossary {
    type Error = QueryError;

    fn try_from(entries: Vec<GlossaryEntry>) -> Result<Self, Self::Error> {
        Self::new(entries)
    }
}

impl From<Glossary> for Vec<GlossaryEntry> {
    fn from(glossary: Glossary) -> Self {
        glossary.entries
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn words(text: &str) -> Vec<String> {
        text.split_whitespace().map(fold).collect()
    }

    fn glossary() -> Glossary {
        Glossary::new(vec![
            GlossaryEntry::approved("ödeme", "payment", TermRelation::Translation).both_ways(),
            GlossaryEntry::approved(
                "abonelik iptali",
                "subscription cancel",
                TermRelation::Translation,
            ),
            GlossaryEntry::approved("abonelik", "subscription", TermRelation::Translation),
            GlossaryEntry::suggested("tahsilat", "charge", TermRelation::Translation),
            GlossaryEntry::approved("kdv", "vat", TermRelation::Abbreviation),
            GlossaryEntry::approved("üye", "member", TermRelation::CodeName)
                .in_domain(Name::new("crm").unwrap()),
        ])
        .unwrap()
    }

    #[test]
    fn longest_phrase_wins() {
        let found = glossary().expand(&words("Abonelik İptali nerede"), None);
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].expansion, "subscription cancel");
        assert_eq!(found[0].matched, "abonelik iptali");
    }

    #[test]
    fn inflected_and_folded_matches() {
        let found = glossary().expand(&words("odemeyi kim yapiyor"), None);
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].expansion, "payment");
        assert!(found[0].inflected);
        // Short terms never match by prefix: `kdvsi` is not `kdv`.
        assert!(glossary().expand(&words("kdvsi"), None).is_empty());
        assert_eq!(glossary().expand(&words("KDV"), None)[0].expansion, "vat");
    }

    #[test]
    fn bidirectional_and_domain() {
        let g = glossary();
        let found = g.expand(&words("payment retry"), None);
        assert_eq!(found[0].expansion, "ödeme");
        assert!(g.expand(&words("üye"), None).is_empty());
        let crm = Name::new("crm").unwrap();
        assert_eq!(g.expand(&words("üye"), Some(&crm))[0].expansion, "member");
    }

    #[test]
    fn suggested_entries_are_reported_with_status() {
        let found = glossary().expand(&words("tahsilat"), None);
        assert_eq!(found[0].status, TermStatus::Suggested);
    }

    #[test]
    fn order_independent() {
        let mut entries = glossary().entries().to_vec();
        entries.reverse();
        assert_eq!(Glossary::new(entries).unwrap(), glossary());
    }

    #[test]
    fn rejects_bad_entries() {
        let bad = [
            GlossaryEntry::approved("", "x", TermRelation::Synonym),
            GlossaryEntry::approved("a b c d e", "x", TermRelation::Synonym),
            GlossaryEntry::approved("same", "SAME", TermRelation::Synonym),
            GlossaryEntry::approved("x\u{7}", "y", TermRelation::Synonym),
        ];
        for entry in bad {
            assert!(Glossary::new(vec![entry]).is_err());
        }
    }
}
