use knowell_core::{ContentHash, LineRange, Name, RepoPath};
use serde::{Deserialize, Serialize};

use crate::{
    CommitId, CoverageGap, Degradation, EdgeKind, EmptyExplanation, EvidenceType, GraphStep,
    Language, Location, Reason, Resolution, SearchResponse, SourceError, ViewId,
};

/// What part of a result to fetch.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SnippetKind {
    /// Signature, doc comment and outline: what the code is, not how.
    Skeleton,
    /// The full text of the range.
    Body,
}

/// A snippet request.
#[derive(Clone, Copy, Debug)]
pub struct SnippetRequest<'a> {
    /// The evidence location (file version, view, range).
    pub location: &'a Location,
    /// Its symbol, if known.
    pub symbol: Option<&'a str>,
    /// Skeleton or body.
    pub kind: SnippetKind,
}

/// Text of one location, read from exactly the cited file version.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Snippet {
    /// The text (already redacted by the source layer).
    pub text: String,
    /// Lines the text covers.
    pub range: LineRange,
    /// Hash of the file version the text was read from; must equal the
    /// location's hash or the snippet is rejected as stale.
    pub content_hash: ContentHash,
}

/// Reads snippets for packing. `Ok(None)` means "no such snippet" (e.g. no
/// skeleton for a plain-text file); an error is reported in the pack.
pub trait SnippetSource {
    /// Fetches one snippet.
    fn snippet(&self, request: &SnippetRequest<'_>) -> Result<Option<Snippet>, SourceError>;
}

/// Counts tokens for the context budget.
pub trait Tokenizer {
    /// Tokens `text` costs.
    fn count_tokens(&self, text: &str) -> u32;
}

/// Approximate tokenizer: one token per `chars` characters, rounded up.
/// The default (4) is a common estimate for code; plug in a real tokenizer
/// where the agent's model is known.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CharsPerToken {
    /// Characters per token (at least 1).
    pub chars: u32,
}

impl Default for CharsPerToken {
    fn default() -> Self {
        Self { chars: 4 }
    }
}

impl Tokenizer for CharsPerToken {
    fn count_tokens(&self, text: &str) -> u32 {
        let per = usize::try_from(self.chars.max(1)).unwrap_or(usize::MAX);
        u32::try_from(text.chars().count().div_ceil(per)).unwrap_or(u32::MAX)
    }
}

/// Exactly which bytes a packed item shows.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct Citation {
    /// Project.
    pub project: Name,
    /// View.
    pub view: ViewId,
    /// Index generation of the view.
    pub generation: u64,
    /// Pinned commit of the view, if any.
    pub commit: Option<CommitId>,
    /// Path relative to the project's source root.
    pub path: RepoPath,
    /// Lines shown.
    pub range: LineRange,
    /// Hash of the file version.
    pub content_hash: ContentHash,
}

impl Citation {
    pub(crate) fn new(location: &Location, commit: Option<&CommitId>, range: LineRange) -> Self {
        Self {
            project: location.project.clone(),
            view: location.view.clone(),
            generation: location.generation,
            commit: commit.cloned(),
            path: location.path.clone(),
            range,
            content_hash: location.content_hash,
        }
    }

    /// `project@view#generation[ commit12] path:L10-L20 hash12` — the header
    /// an agent sees; its tokens count against the budget.
    pub fn label(&self) -> String {
        let commit = self
            .commit
            .as_ref()
            .map_or_else(String::new, |c| format!(" {}", c.short()));
        format!(
            "{}@{}#{}{commit} {}:{} {}",
            self.project,
            self.view,
            self.generation,
            self.path,
            self.range,
            self.content_hash.short()
        )
    }
}

/// Where a packed item came from.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(tag = "from", rename_all = "snake_case")]
pub enum Origin {
    /// A ranked result.
    Result {
        /// Its rank.
        rank: u32,
    },
    /// A graph-expanded item.
    Expanded {
        /// Rank of its seed result.
        seed_rank: u32,
        /// Hops from the seed.
        depth: u32,
    },
}

/// One packed piece of context. Its text is untrusted repository data.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct PackItem {
    /// Result or expanded item it came from.
    pub origin: Origin,
    /// Skeleton or body.
    pub kind: SnippetKind,
    /// Exactly what is shown.
    pub citation: Citation,
    /// Symbol, if known.
    pub symbol: Option<String>,
    /// The text.
    pub text: String,
    /// Tokens charged: citation label plus text.
    pub tokens: u32,
    /// Why the item is relevant.
    pub why: Vec<Reason>,
    /// Other items whose text lies inside this one (and so are not repeated).
    pub covers: Vec<Citation>,
}

/// Why something was left out of the pack.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum OmitReason {
    /// It did not fit the remaining budget.
    OverBudget {
        /// Tokens it would have needed (net of text it would replace).
        needed_tokens: u32,
        /// Tokens left at that point.
        remaining_tokens: u32,
    },
    /// Its text overlaps text already packed.
    DuplicateOf {
        /// The packed item it overlaps.
        citation: Citation,
    },
    /// Its text lies entirely inside another packed item.
    CoveredBy {
        /// The packed item containing it.
        citation: Citation,
    },
    /// The snippet came from a different file version than the result.
    StaleContent {
        /// Hash the result cites.
        expected: ContentHash,
        /// Hash the snippet source returned.
        found: ContentHash,
    },
    /// No snippet of any kind was available.
    NoSnippet,
    /// The snippet source failed.
    SnippetFailed {
        /// Its error message.
        message: String,
    },
}

/// Something left out of the pack.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Omission {
    /// Result or expanded item.
    pub origin: Origin,
    /// Its location.
    pub location: Location,
    /// Its symbol, if known.
    pub symbol: Option<String>,
    /// Which snippet was left out; `None` when the whole item was.
    pub kind: Option<SnippetKind>,
    /// Why.
    pub reason: OmitReason,
}

/// What the agent should not take for granted about the pack.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Uncertainty {
    /// A pipeline part was missing or failing.
    Degraded {
        /// Which, and why.
        degradation: Degradation,
    },
    /// References are not resolved for this language in this project.
    NoReferenceResolution {
        /// Project.
        project: Name,
        /// Language.
        language: Language,
    },
    /// A graph path rests on heuristic or model-suggested evidence, or on an
    /// ambiguous / unresolved edge.
    WeakGraphEvidence {
        /// The item the path leads to.
        location: Location,
        /// The first weak edge.
        edge: EdgeKind,
        /// Its evidence type.
        evidence: EvidenceType,
        /// Its resolution status.
        resolution: Resolution,
    },
    /// Graph expansion stopped at its node budget; more neighbours exist.
    ExpansionBudgetExhausted {
        /// The budget.
        node_budget: usize,
    },
    /// More results existed than the result limit allowed.
    MoreResults {
        /// How many were cut.
        truncated: usize,
    },
    /// The search found nothing; this says why.
    NoResults {
        /// The empty-result explanation.
        explanation: EmptyExplanation,
    },
}

/// Budgeted, cited context for an agent.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ContextPack {
    /// The budget, in tokens.
    pub budget_tokens: u32,
    /// Tokens used (never above the budget).
    pub used_tokens: u32,
    /// Packed items, in result order (results by rank, then expanded items).
    pub items: Vec<PackItem>,
    /// What was left out, and why.
    pub omitted: Vec<Omission>,
    /// What the agent should not take for granted.
    pub uncertainties: Vec<Uncertainty>,
}

/// Packs `response` into `budget_tokens` with the default ≈4 chars/token estimate.
pub fn pack(
    response: &SearchResponse,
    budget_tokens: u32,
    snippets: &dyn SnippetSource,
) -> ContextPack {
    pack_with(response, budget_tokens, snippets, &CharsPerToken::default())
}

struct Packed {
    kind: SnippetKind,
    range: LineRange,
    text: String,
    tokens: u32,
    covers: Vec<Citation>,
}

struct Entry<'a> {
    origin: Origin,
    location: &'a Location,
    commit: Option<&'a CommitId>,
    symbol: Option<&'a str>,
    why: &'a [Reason],
    packed: Option<Packed>,
    /// No further snippet is tried (stale, failed, duplicate, covered).
    closed: bool,
}

enum Fetched {
    Found(Snippet),
    Missing,
    Stale(ContentHash),
    Failed(String),
}

fn fetch(snippets: &dyn SnippetSource, entry: &Entry<'_>, kind: SnippetKind) -> Fetched {
    let request = SnippetRequest {
        location: entry.location,
        symbol: entry.symbol,
        kind,
    };
    match snippets.snippet(&request) {
        Ok(Some(snippet)) if snippet.content_hash != entry.location.content_hash => {
            Fetched::Stale(snippet.content_hash)
        }
        Ok(Some(snippet)) if !snippet.text.is_empty() => Fetched::Found(snippet),
        Ok(_) => Fetched::Missing,
        Err(error) => Fetched::Failed(error.to_string()),
    }
}

fn contains(outer: LineRange, inner: LineRange) -> bool {
    outer.start() <= inner.start() && inner.end() <= outer.end()
}

/// Packs the results (then the expanded items) into `budget_tokens`:
///
/// 1. **Skeletons first**, in rank order, so the agent sees what every
///    relevant piece *is* before any budget goes to how it works.
/// 2. **Bodies by rank**: each item is upgraded to its full text if the
///    remaining budget allows (net of the skeleton it replaces and of other
///    packed text it contains, which is then not repeated).
/// 3. Overlapping text in the same pinned file occurrence is never sent twice.
///    Equal bytes at another path, project, view or generation retain their
///    own citation; anything that does not fit is listed in `omitted`.
///
/// Items that do not fit are skipped, and smaller later items may still fit.
/// The result is deterministic for the same inputs.
pub fn pack_with(
    response: &SearchResponse,
    budget_tokens: u32,
    snippets: &dyn SnippetSource,
    tokenizer: &dyn Tokenizer,
) -> ContextPack {
    let mut entries: Vec<Entry<'_>> = response
        .results
        .iter()
        .map(|r| Entry {
            origin: Origin::Result { rank: r.rank },
            location: &r.location,
            commit: r.commit.as_ref(),
            symbol: r.symbol.as_deref(),
            why: &r.why,
            packed: None,
            closed: false,
        })
        .chain(response.expanded.iter().map(|e| Entry {
            origin: Origin::Expanded {
                seed_rank: e.seed_rank,
                depth: e.depth,
            },
            location: &e.location,
            commit: e.commit.as_ref(),
            symbol: e.symbol.as_deref(),
            why: &e.why,
            packed: None,
            closed: false,
        }))
        .collect();
    let mut packer = Packer {
        budget: budget_tokens,
        used: 0,
        omitted: Vec::new(),
        tokenizer,
    };
    for i in 0..entries.len() {
        packer.skeleton(&mut entries, i, snippets);
    }
    for i in 0..entries.len() {
        packer.body(&mut entries, i, snippets);
    }

    let items = entries
        .iter()
        .filter_map(|entry| {
            let packed = entry.packed.as_ref()?;
            Some(PackItem {
                origin: entry.origin,
                kind: packed.kind,
                citation: Citation::new(entry.location, entry.commit, packed.range),
                symbol: entry.symbol.map(str::to_owned),
                text: packed.text.clone(),
                tokens: packed.tokens,
                why: entry.why.to_vec(),
                covers: packed.covers.clone(),
            })
        })
        .collect();
    ContextPack {
        budget_tokens,
        used_tokens: packer.used,
        items,
        omitted: packer.omitted,
        uncertainties: uncertainties(response),
    }
}

struct Packer<'t> {
    budget: u32,
    used: u32,
    omitted: Vec<Omission>,
    tokenizer: &'t dyn Tokenizer,
}

impl Packer<'_> {
    fn remaining(&self) -> u32 {
        self.budget.saturating_sub(self.used)
    }

    fn cost(&self, entry: &Entry<'_>, snippet: &Snippet) -> u32 {
        let label = Citation::new(entry.location, entry.commit, snippet.range).label();
        self.tokenizer
            .count_tokens(&label)
            .saturating_add(self.tokenizer.count_tokens(&snippet.text))
    }

    fn omit(&mut self, entry: &Entry<'_>, kind: Option<SnippetKind>, reason: OmitReason) {
        self.omitted.push(Omission {
            origin: entry.origin,
            location: entry.location.clone(),
            symbol: entry.symbol.map(str::to_owned),
            kind,
            reason,
        });
    }

    fn citation_of(entries: &[Entry<'_>], j: usize) -> Option<Citation> {
        let entry = entries.get(j)?;
        let packed = entry.packed.as_ref()?;
        Some(Citation::new(entry.location, entry.commit, packed.range))
    }

    fn skeleton(&mut self, entries: &mut [Entry<'_>], i: usize, snippets: &dyn SnippetSource) {
        let Some(entry) = entries.get(i) else {
            return;
        };
        let snippet = match fetch(snippets, entry, SnippetKind::Skeleton) {
            Fetched::Found(snippet) => snippet,
            Fetched::Missing => return,
            Fetched::Stale(found) => {
                let expected = entry.location.content_hash;
                self.omit(entry, None, OmitReason::StaleContent { expected, found });
                if let Some(entry) = entries.get_mut(i) {
                    entry.closed = true;
                }
                return;
            }
            Fetched::Failed(message) => {
                self.omit(entry, None, OmitReason::SnippetFailed { message });
                if let Some(entry) = entries.get_mut(i) {
                    entry.closed = true;
                }
                return;
            }
        };
        let duplicate = entries.iter().enumerate().find_map(|(j, other)| {
            let packed = other.packed.as_ref()?;
            // Deduplicate shown text within one binding, never by payload
            // equality: another occurrence can have different live relations.
            let same = j != i
                && other.location.same_file(entry.location)
                && (packed.range == snippet.range
                    || (packed.kind == SnippetKind::Body && contains(packed.range, snippet.range)));
            same.then_some(j)
        });
        if let Some(j) = duplicate
            && let Some(citation) = Self::citation_of(entries, j)
        {
            self.omit(entry, None, OmitReason::DuplicateOf { citation });
            if let Some(entry) = entries.get_mut(i) {
                entry.closed = true;
            }
            return;
        }
        let cost = self.cost(entry, &snippet);
        if cost > self.remaining() {
            let reason = OmitReason::OverBudget {
                needed_tokens: cost,
                remaining_tokens: self.remaining(),
            };
            self.omit(entry, None, reason);
            if let Some(entry) = entries.get_mut(i) {
                entry.closed = true;
            }
            return;
        }
        self.used = self.used.saturating_add(cost);
        if let Some(entry) = entries.get_mut(i) {
            entry.packed = Some(Packed {
                kind: SnippetKind::Skeleton,
                range: snippet.range,
                text: snippet.text,
                tokens: cost,
                covers: Vec::new(),
            });
        }
    }

    fn body(&mut self, entries: &mut [Entry<'_>], i: usize, snippets: &dyn SnippetSource) {
        let Some(entry) = entries.get(i) else {
            return;
        };
        if entry.closed {
            return;
        }
        let has_skeleton = entry.packed.is_some();
        let body = match fetch(snippets, entry, SnippetKind::Body) {
            Fetched::Found(body) => body,
            Fetched::Missing => {
                if !has_skeleton {
                    self.omit(entry, None, OmitReason::NoSnippet);
                }
                return;
            }
            Fetched::Stale(found) => {
                let expected = entry.location.content_hash;
                let kind = has_skeleton.then_some(SnippetKind::Body);
                self.omit(entry, kind, OmitReason::StaleContent { expected, found });
                return;
            }
            Fetched::Failed(message) => {
                let kind = has_skeleton.then_some(SnippetKind::Body);
                self.omit(entry, kind, OmitReason::SnippetFailed { message });
                return;
            }
        };
        if let Some(own) = &entry.packed
            && own.range == body.range
            && own.text == body.text
        {
            return;
        }

        let mut contained = Vec::new();
        let mut covered_by = None;
        let mut conflict = None;
        for (j, other) in entries.iter().enumerate() {
            let Some(packed) = other.packed.as_ref() else {
                continue;
            };
            // Covers and overlap omissions certify this exact source binding.
            if j == i || !other.location.same_file(entry.location) {
                continue;
            }
            if contains(body.range, packed.range) {
                contained.push(j);
            } else if packed.kind == SnippetKind::Body && contains(packed.range, body.range) {
                covered_by = covered_by.or(Some(j));
            } else if packed.kind == SnippetKind::Body && packed.range.overlaps(&body.range) {
                conflict = conflict.or(Some(j));
            }
        }

        if let Some(j) = covered_by
            && let Some(citation) = Self::citation_of(entries, j)
        {
            // The body is already inside another packed body: drop the
            // skeleton too and refund it.
            let refund = entry.packed.as_ref().map_or(0, |p| p.tokens);
            self.used = self.used.saturating_sub(refund);
            self.omit(entry, None, OmitReason::CoveredBy { citation });
            if let Some(entry) = entries.get_mut(i) {
                entry.packed = None;
                entry.closed = true;
            }
            return;
        }
        if let Some(j) = conflict
            && let Some(citation) = Self::citation_of(entries, j)
        {
            let kind = has_skeleton.then_some(SnippetKind::Body);
            self.omit(entry, kind, OmitReason::DuplicateOf { citation });
            return;
        }

        let cost = self.cost(entry, &body);
        let own = entry.packed.as_ref().map_or(0, |p| p.tokens);
        let refund = contained
            .iter()
            .filter_map(|j| entries.get(*j)?.packed.as_ref().map(|p| p.tokens))
            .fold(own, u32::saturating_add);
        let net = cost.saturating_sub(refund);
        if net > self.remaining() {
            let kind = has_skeleton.then_some(SnippetKind::Body);
            let reason = OmitReason::OverBudget {
                needed_tokens: net,
                remaining_tokens: self.remaining(),
            };
            self.omit(entry, kind, reason);
            return;
        }

        let mut covers = Vec::new();
        for j in &contained {
            if let Some(citation) = Self::citation_of(entries, *j) {
                covers.push(citation);
            }
            if let Some(other) = entries.get_mut(*j) {
                if let Some(packed) = other.packed.take() {
                    covers.extend(packed.covers);
                }
                other.closed = true;
            }
        }
        self.used = self.used.saturating_sub(refund).saturating_add(cost);
        if let Some(entry) = entries.get_mut(i) {
            entry.packed = Some(Packed {
                kind: SnippetKind::Body,
                range: body.range,
                text: body.text,
                tokens: cost,
                covers,
            });
        }
    }
}

fn first_weak_step(steps: &[GraphStep]) -> Option<&GraphStep> {
    steps.iter().find(|s| s.is_uncertain())
}

pub(crate) fn uncertainties(response: &SearchResponse) -> Vec<Uncertainty> {
    let mut out: Vec<Uncertainty> = response
        .degraded
        .iter()
        .map(|d| Uncertainty::Degraded {
            degradation: d.clone(),
        })
        .collect();
    for gap in &response.coverage_gaps {
        let CoverageGap::NoReferenceResolution { project, language } = gap;
        out.push(Uncertainty::NoReferenceResolution {
            project: project.clone(),
            language: language.clone(),
        });
    }
    let weak = |location: &Location, step: &GraphStep| Uncertainty::WeakGraphEvidence {
        location: location.clone(),
        edge: step.edge,
        evidence: step.evidence,
        resolution: step.resolution,
    };
    for result in &response.results {
        for reason in &result.why {
            if let Reason::GraphPath { steps, .. } = reason
                && let Some(step) = first_weak_step(steps)
            {
                out.push(weak(&result.location, step));
            }
        }
    }
    for item in &response.expanded {
        if let Some(step) = first_weak_step(&item.path) {
            out.push(weak(&item.location, step));
        }
    }
    if response.stats.expansion.budget_exhausted {
        out.push(Uncertainty::ExpansionBudgetExhausted {
            node_budget: response.stats.expansion.node_budget,
        });
    }
    if response.stats.truncated_by_limit > 0 {
        out.push(Uncertainty::MoreResults {
            truncated: response.stats.truncated_by_limit,
        });
    }
    if let Some(explanation) = &response.empty {
        out.push(Uncertainty::NoResults {
            explanation: explanation.clone(),
        });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chars_per_token_rounds_up() {
        let t = CharsPerToken::default();
        assert_eq!(t.count_tokens(""), 0);
        assert_eq!(t.count_tokens("abc"), 1);
        assert_eq!(t.count_tokens("abcd"), 1);
        assert_eq!(t.count_tokens("abcde"), 2);
        assert_eq!(t.count_tokens("ödeme"), 2, "counts characters, not bytes");
        assert_eq!(
            CharsPerToken { chars: 0 }.count_tokens("ab"),
            2,
            "0 is treated as 1"
        );
    }
}
