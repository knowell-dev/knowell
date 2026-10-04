//! Bounded source selection. Content similarity is a selection penalty, never
//! a claim that equal text at another occurrence has the same dependencies.

use std::collections::BTreeSet;

use knowell_core::LineRange;

use super::{Candidate, CandidateSource, TaskPackOptions, empty_pack};
use crate::{
    Citation, CommitId, ContextPack, Omission, OmitReason, PackItem, Reason, SearchResponse,
    Snippet, SnippetKind, SnippetRequest, SnippetSource, Tokenizer,
};

const FEATURE_LIMIT: usize = 128;
const TERM_CHARS: usize = 128;
const WINDOW_CONTEXT_LINES: usize = 8;

pub(super) struct Outcome {
    pub pack: ContextPack,
    pub evaluations: usize,
    pub exhausted: bool,
}

#[derive(Clone, Default)]
struct Signals {
    matched: BTreeSet<String>,
    fingerprints: BTreeSet<u64>,
}

struct Prepared<'a> {
    candidate: &'a Candidate<'a>,
    commit: Option<&'a CommitId>,
    symbol: Option<&'a str>,
    why: &'a [Reason],
    snippet: Snippet,
    signals: Signals,
    cost: u32,
    exact: bool,
}

struct Assessment {
    index: usize,
    parts: Vec<Snippet>,
    signals: Signals,
    cost: u32,
    value: f64,
}

pub(super) fn pack(
    response: &SearchResponse,
    budget: u32,
    source: &dyn SnippetSource,
    tokenizer: &dyn Tokenizer,
    options: &TaskPackOptions,
    candidates: &[Candidate<'_>],
) -> Outcome {
    let query: BTreeSet<_> = response
        .plan
        .lexical_terms()
        .iter()
        .flat_map(|text| terms(text))
        .take(FEATURE_LIMIT)
        .collect();
    let mut pack = empty_pack(response, budget);
    let mut prepared = Vec::new();
    for candidate in candidates {
        let (commit, symbol, why) = match candidate.source {
            CandidateSource::Result(index) => {
                let Some(item) = response.results.get(index) else {
                    continue;
                };
                (
                    item.commit.as_ref(),
                    item.symbol.as_deref(),
                    item.why.as_slice(),
                )
            }
            CandidateSource::Expanded(index) => {
                let Some(item) = response.expanded.get(index) else {
                    continue;
                };
                (
                    item.commit.as_ref(),
                    item.symbol.as_deref(),
                    item.why.as_slice(),
                )
            }
        };
        let request = SnippetRequest {
            location: candidate.location,
            symbol,
            kind: SnippetKind::Body,
        };
        let snippet = match source.snippet(&request) {
            Ok(Some(snippet)) if snippet.content_hash != candidate.location.content_hash => {
                pack.omitted.push(omission(
                    candidate,
                    symbol,
                    OmitReason::StaleContent {
                        expected: candidate.location.content_hash,
                        found: snippet.content_hash,
                    },
                ));
                continue;
            }
            Ok(Some(snippet)) if !snippet.text.trim().is_empty() => snippet,
            Ok(_) => {
                pack.omitted
                    .push(omission(candidate, symbol, OmitReason::NoSnippet));
                continue;
            }
            Err(error) => {
                pack.omitted.push(omission(
                    candidate,
                    symbol,
                    OmitReason::SnippetFailed {
                        message: error.to_string(),
                    },
                ));
                continue;
            }
        };
        if exact_lines(&snippet).is_none() {
            pack.omitted.push(omission(
                candidate,
                symbol,
                OmitReason::SnippetFailed {
                    message: "source snippet line count does not match its cited range".into(),
                },
            ));
            continue;
        }
        let signals = signals(&snippet.text, &query);
        let cost = cost(candidate, commit, &snippet, tokenizer);
        let exact = why.iter().any(|reason| {
            let Reason::ExactMatch { term, .. } = reason else {
                return false;
            };
            response
                .plan
                .exact_terms
                .iter()
                .any(|requested| crate::text::fold(&requested.text) == crate::text::fold(term))
        });
        prepared.push(Prepared {
            candidate,
            commit,
            symbol,
            why,
            snippet,
            signals,
            cost,
            exact,
        });
    }

    let mut remaining: BTreeSet<_> = (0..prepared.len()).collect();
    let mut emitted_signals = Vec::new();
    let mut matched = BTreeSet::new();
    let mut evaluations = 0_usize;
    let mut exhausted = false;
    loop {
        let mut best: Option<Assessment> = None;
        let available = budget.saturating_sub(pack.used_tokens);
        for index in remaining.iter().copied() {
            if evaluations >= options.evaluation_budget {
                exhausted = true;
                break;
            }
            evaluations = evaluations.saturating_add(1);
            let Some(candidate) = prepared.get(index) else {
                continue;
            };
            let parts: Vec<_> = uncovered(candidate, &pack.items)
                .into_iter()
                .filter(|part| !part.text.trim().is_empty())
                .collect();
            if parts.is_empty() {
                continue;
            }
            let complete = parts.len() == 1
                && parts
                    .first()
                    .is_some_and(|part| part.range == candidate.snippet.range);
            let mut tokens = if complete {
                candidate.cost
            } else {
                parts.iter().fold(0_u32, |total, part| {
                    total.saturating_add(cost(
                        candidate.candidate,
                        candidate.commit,
                        part,
                        tokenizer,
                    ))
                })
            };
            let mut partial = false;
            let parts = if tokens > available {
                let Some(window) =
                    window(candidate, &parts, &query, &matched, available, tokenizer)
                else {
                    continue;
                };
                tokens = cost(candidate.candidate, candidate.commit, &window, tokenizer);
                partial = true;
                vec![window]
            } else {
                parts
            };
            let content = if complete && !partial {
                candidate.signals.clone()
            } else {
                combined_signals(&parts, &query)
            };
            let redundancy = emitted_signals
                .iter()
                .map(|previous: &Signals| similarity(&content.fingerprints, &previous.fingerprints))
                .fold(0.0_f64, f64::max);
            let denominator = number(query.len().max(1));
            let new_terms = number(content.matched.difference(&matched).count()) / denominator;
            let relevance = candidate.candidate.relevance;
            let content_match = number(content.matched.len()) / denominator;
            let already_emitted = pack
                .items
                .iter()
                .any(|item| item.origin == candidate.candidate.origin);
            let linked = candidate.candidate.paths.iter().any(|(seed, steps)| {
                !steps.is_empty()
                    && steps.iter().all(|step| !step.is_uncertain())
                    && super::body(&pack, seed).is_some()
            });
            // These uncalibrated coefficients are explicit heuristics. In
            // particular, matching a term is not proof of answering a question.
            let gain = 0.6 * relevance * (1.0 + content_match) * (1.0 - redundancy)
                + 0.9 * new_terms
                + if linked && !already_emitted {
                    0.25 * relevance
                } else {
                    0.0
                }
                + if candidate.exact && !already_emitted {
                    0.25 + 0.25 * relevance
                } else {
                    0.0
                }
                - 0.2 * (f64::from(tokens) / f64::from(budget.max(1))).sqrt();
            if gain <= 0.0 {
                continue;
            }
            let value = gain / f64::from(tokens.max(1)).sqrt();
            let better = best.as_ref().is_none_or(|previous| {
                value.total_cmp(&previous.value).is_gt()
                    || (value.total_cmp(&previous.value).is_eq()
                        && (tokens, index) < (previous.cost, previous.index))
            });
            if better {
                best = Some(Assessment {
                    index,
                    parts,
                    signals: content,
                    cost: tokens,
                    value,
                });
            }
        }
        let Some(selected) = best else {
            break;
        };
        let Some(candidate) = prepared.get(selected.index) else {
            break;
        };
        for part in selected.parts {
            let citation =
                Citation::new(candidate.candidate.location, candidate.commit, part.range);
            let tokens = cost(candidate.candidate, candidate.commit, &part, tokenizer);
            pack.items.push(PackItem {
                origin: candidate.candidate.origin,
                kind: SnippetKind::Body,
                citation,
                symbol: candidate.symbol.map(str::to_owned),
                text: part.text,
                tokens,
                why: candidate.why.to_vec(),
                covers: Vec::new(),
            });
        }
        pack.used_tokens = pack.used_tokens.saturating_add(selected.cost);
        matched.extend(selected.signals.matched.iter().cloned());
        emitted_signals.push(selected.signals);
        if fully_emitted(candidate, &pack.items) {
            remaining.remove(&selected.index);
        }
        if exhausted || remaining.is_empty() || pack.used_tokens >= budget {
            break;
        }
    }

    for candidate in &prepared {
        if let Some(item) = pack.items.iter().find(|item| {
            same_file(candidate, &item.citation)
                && contains(item.citation.range, candidate.snippet.range)
        }) {
            if item.origin != candidate.candidate.origin {
                pack.omitted.push(omission(
                    candidate.candidate,
                    candidate.symbol,
                    OmitReason::CoveredBy {
                        citation: item.citation.clone(),
                    },
                ));
            }
        } else if !fully_emitted(candidate, &pack.items)
            && candidate.cost > budget.saturating_sub(pack.used_tokens)
        {
            // An excerpt does not certify the original body. Record its full
            // cost too, so diagnostics can distinguish body clipping from recall.
            pack.omitted.push(omission(
                candidate.candidate,
                candidate.symbol,
                OmitReason::OverBudget {
                    needed_tokens: candidate.cost,
                    remaining_tokens: budget.saturating_sub(pack.used_tokens),
                },
            ));
        }
    }
    pack.items.sort_by(|a, b| {
        a.origin
            .cmp(&b.origin)
            .then_with(|| a.citation.cmp(&b.citation))
    });
    Outcome {
        pack,
        evaluations,
        exhausted,
    }
}

fn omission(candidate: &Candidate<'_>, symbol: Option<&str>, reason: OmitReason) -> Omission {
    Omission {
        origin: candidate.origin,
        location: candidate.location.clone(),
        symbol: symbol.map(str::to_owned),
        kind: Some(SnippetKind::Body),
        reason,
    }
}

fn cost(
    candidate: &Candidate<'_>,
    commit: Option<&CommitId>,
    snippet: &Snippet,
    tokenizer: &dyn Tokenizer,
) -> u32 {
    tokenizer
        .count_tokens(&Citation::new(candidate.location, commit, snippet.range).label())
        .saturating_add(tokenizer.count_tokens(&snippet.text))
}

fn same_file(candidate: &Prepared<'_>, citation: &Citation) -> bool {
    let location = candidate.candidate.location;
    location.project == citation.project
        && location.view == citation.view
        && location.generation == citation.generation
        && location.path == citation.path
        && location.content_hash == citation.content_hash
        && candidate.commit == citation.commit.as_ref()
}

fn contains(outer: LineRange, inner: LineRange) -> bool {
    outer.start() <= inner.start() && inner.end() <= outer.end()
}

fn number(value: usize) -> f64 {
    f64::from(u32::try_from(value).unwrap_or(u32::MAX))
}

fn similarity(a: &BTreeSet<u64>, b: &BTreeSet<u64>) -> f64 {
    if a.is_empty() || b.is_empty() {
        return 0.0;
    }
    number(a.intersection(b).count()) / number(a.union(b).count().max(1))
}

fn signals(text: &str, query: &BTreeSet<String>) -> Signals {
    let mut out = Signals::default();
    for term in terms(text) {
        if query.contains(&term) {
            out.matched.insert(term.clone());
        }
        // Keeping the smallest stable fingerprints samples the complete body,
        // including late lines, without storing unbounded identifier sets.
        let fingerprint = term.bytes().fold(0xcbf2_9ce4_8422_2325_u64, |hash, byte| {
            (hash ^ u64::from(byte)).wrapping_mul(0x0000_0100_0000_01b3)
        });
        out.fingerprints.insert(fingerprint);
        if out.fingerprints.len() > FEATURE_LIMIT {
            out.fingerprints.pop_last();
        }
    }
    for line in text.lines().filter(|line| !line.trim().is_empty()) {
        // Line features retain operators, literal values and token order that
        // an identifier-only bag would erase. They still establish similarity,
        // never semantic equivalence or proof that an omitted binding is covered.
        let fingerprint = line
            .chars()
            .filter(|c| !c.is_whitespace())
            .fold(0x8422_2325_cbf2_9ce4_u64, |hash, c| {
                (hash ^ u64::from(u32::from(c))).wrapping_mul(0x0000_0100_0000_01b3)
            });
        out.fingerprints.insert(fingerprint);
        if out.fingerprints.len() > FEATURE_LIMIT {
            out.fingerprints.pop_last();
        }
    }
    out
}

fn combined_signals(parts: &[Snippet], query: &BTreeSet<String>) -> Signals {
    let mut out = Signals::default();
    for part in parts {
        let part = signals(&part.text, query);
        out.matched.extend(part.matched);
        out.fingerprints.extend(part.fingerprints);
        while out.fingerprints.len() > FEATURE_LIMIT {
            out.fingerprints.pop_last();
        }
    }
    out
}

fn terms(text: &str) -> impl Iterator<Item = String> + '_ {
    text.split(|c: char| !c.is_alphanumeric())
        .filter(|term| {
            let length = term.chars().take(TERM_CHARS + 1).count();
            (1..=TERM_CHARS).contains(&length)
        })
        .flat_map(|term| {
            let mut parts = vec![crate::text::fold(term)];
            let mut piece = String::new();
            let mut lower = false;
            for c in term.chars() {
                if c.is_uppercase() && lower {
                    parts.push(crate::text::fold(&piece));
                    piece.clear();
                }
                piece.push(c);
                lower = c.is_lowercase();
            }
            if !piece.is_empty() {
                parts.push(crate::text::fold(&piece));
            }
            parts
        })
        .filter(|term| !crate::text::is_stopword(term))
}

fn exact_lines(snippet: &Snippet) -> Option<Vec<&str>> {
    let mut lines: Vec<_> = snippet.text.split_inclusive('\n').collect();
    let expected = snippet
        .range
        .end()
        .checked_sub(snippet.range.start())?
        .checked_add(1)?;
    if snippet.text.ends_with('\n') && u32::try_from(lines.len()).ok()?.checked_add(1)? == expected
    {
        // A logical-line adapter can omit the last empty line's terminator.
        // Keep that line without manufacturing bytes that were not returned.
        lines.push("");
    }
    (u32::try_from(lines.len()).ok()? == expected).then_some(lines)
}

fn slice(snippet: &Snippet, lines: &[&str], start: usize, end: usize) -> Option<Snippet> {
    let span = lines.get(start..=end)?;
    let first = snippet
        .range
        .start()
        .checked_add(u32::try_from(start).ok()?)?;
    let last = snippet
        .range
        .start()
        .checked_add(u32::try_from(end).ok()?)?;
    Some(Snippet {
        text: span.concat(),
        range: LineRange::new(first, last).ok()?,
        content_hash: snippet.content_hash,
    })
}

fn uncovered(candidate: &Prepared<'_>, selected: &[PackItem]) -> Vec<Snippet> {
    let overlaps: Vec<_> = selected
        .iter()
        .filter(|item| {
            same_file(candidate, &item.citation)
                && item.citation.range.overlaps(&candidate.snippet.range)
        })
        .collect();
    if overlaps.is_empty() {
        return vec![candidate.snippet.clone()];
    }
    if overlaps
        .iter()
        .any(|item| contains(item.citation.range, candidate.snippet.range))
    {
        return Vec::new();
    }
    let Some(lines) = exact_lines(&candidate.snippet) else {
        // A source with an inconsistent line count cannot be split honestly.
        return Vec::new();
    };
    let mut spans = vec![(
        candidate.snippet.range.start(),
        candidate.snippet.range.end(),
    )];
    for item in overlaps {
        let range = item.citation.range;
        let mut next = Vec::new();
        for (start, end) in spans {
            if range.end() < start || range.start() > end {
                next.push((start, end));
                continue;
            }
            if start < range.start() {
                next.push((start, range.start().saturating_sub(1)));
            }
            if range.end() < end {
                next.push((range.end().saturating_add(1), end));
            }
        }
        spans = next;
    }
    spans
        .into_iter()
        .filter_map(|(start, end)| {
            let first =
                usize::try_from(start.checked_sub(candidate.snippet.range.start())?).ok()?;
            let last = usize::try_from(end.checked_sub(candidate.snippet.range.start())?).ok()?;
            slice(&candidate.snippet, &lines, first, last)
        })
        .collect()
}

fn fully_emitted(candidate: &Prepared<'_>, selected: &[PackItem]) -> bool {
    let mut ranges: Vec<_> = selected
        .iter()
        .filter(|item| same_file(candidate, &item.citation))
        .map(|item| item.citation.range)
        .collect();
    ranges.sort_by_key(|range| (range.start(), range.end()));
    let mut next = u64::from(candidate.snippet.range.start());
    let end = u64::from(candidate.snippet.range.end());
    for range in ranges {
        if u64::from(range.end()) < next {
            continue;
        }
        if u64::from(range.start()) > next {
            return false;
        }
        next = next.max(u64::from(range.end()).saturating_add(1));
        if next > end {
            return true;
        }
    }
    false
}

fn window(
    candidate: &Prepared<'_>,
    parts: &[Snippet],
    query: &BTreeSet<String>,
    emitted: &BTreeSet<String>,
    available: u32,
    tokenizer: &dyn Tokenizer,
) -> Option<Snippet> {
    let indexed: Vec<_> = parts
        .iter()
        .filter_map(|part| exact_lines(part).map(|lines| (part, lines)))
        .collect();
    let mut anchors = Vec::new();
    for (part_index, (_, lines)) in indexed.iter().enumerate() {
        for (line_index, line) in lines.iter().enumerate() {
            let matched: BTreeSet<_> = terms(line).filter(|term| query.contains(term)).collect();
            let value = (matched.difference(emitted).count(), matched.len());
            if value.1 > 0 {
                anchors.push((part_index, line_index, value));
            }
        }
    }
    anchors.sort_by(|a, b| b.2.cmp(&a.2).then_with(|| (a.0, a.1).cmp(&(b.0, b.1))));
    for (part_index, center, _) in anchors {
        let (part, lines) = indexed.get(part_index)?;
        let mut start = center.saturating_sub(WINDOW_CONTEXT_LINES);
        let mut end = center
            .saturating_add(WINDOW_CONTEXT_LINES)
            .min(lines.len().saturating_sub(1));
        loop {
            let snippet = slice(part, lines, start, end)?;
            if cost(candidate.candidate, candidate.commit, &snippet, tokenizer) <= available {
                return Some(snippet);
            }
            if start == center && end == center {
                break;
            }
            if end.saturating_sub(center) > center.saturating_sub(start) {
                end = end.saturating_sub(1);
            } else {
                start = start.saturating_add(1);
            }
        }
    }
    None
}
