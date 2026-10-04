//! Deterministic source windows. Every returned range addresses contiguous
//! original lines; no summary or synthetic signature is inserted into a body.

use std::collections::{BTreeMap, BTreeSet, VecDeque};

use knowell_core::LineRange;

use crate::snapshot::{line_count, terms_of};

/// Bound query-side work independently of the length of a natural-language task.
const MAX_REGION_TERMS: usize = 64;
/// An excerpt search does not scan an unbounded enclosing declaration.
const MAX_SCAN_LINES: u32 = 20_000;

/// Byte-based token estimate with conservative source-block framing allowance.
/// The renderer enforces the final whole-response byte cap independently.
pub(crate) struct SourceBudgetCounter;

impl knowell_query::Tokenizer for SourceBudgetCounter {
    fn count_tokens(&self, text: &str) -> u32 {
        u32::try_from(text.len().div_ceil(4))
            .unwrap_or(u32::MAX)
            .saturating_add(36)
    }
}

/// Shared scope and limitation text must fit alongside selected source blocks.
pub(crate) fn source_budget(requested: u32) -> u32 {
    requested.saturating_sub(128)
}

/// Exact bytes of contiguous source lines, including their existing LF/CRLF
/// terminators. A trailing newline does not create another logical line.
pub(crate) fn slice_source_lines(text: &str, range: LineRange) -> String {
    let skip = usize::try_from(range.start().saturating_sub(1)).unwrap_or(usize::MAX);
    let count = usize::try_from(range.line_count()).unwrap_or(usize::MAX);
    text.split_inclusive('\n').skip(skip).take(count).collect()
}

/// Normalized lexical cues for choosing a region, never a semantic sufficiency
/// test. Embedding retrieval remains responsible for cross-language candidates.
pub(crate) fn region_terms(query: &str) -> BTreeSet<String> {
    terms_of(query)
        .into_iter()
        .filter(|term| {
            term.chars().count() >= 3
                && !matches!(
                    term.as_str(),
                    "the"
                        | "and"
                        | "for"
                        | "with"
                        | "how"
                        | "where"
                        | "what"
                        | "does"
                        | "this"
                        | "that"
                        | "code"
                        | "find"
                        | "show"
                        | "nasıl"
                        | "nerede"
                        | "hangi"
                        | "için"
                        | "olan"
                        | "bir"
                )
        })
        .take(MAX_REGION_TERMS)
        .collect()
}

/// Smallest enclosing declaration that contains a source line.
pub(crate) fn enclosing_at(
    declarations: impl Iterator<Item = LineRange>,
    line: u32,
) -> Option<LineRange> {
    declarations
        .filter(|range| range.start() <= line && line <= range.end())
        .min_by_key(|range| (range.line_count(), range.start(), range.end()))
}

/// A source line with the most distinct query cues in a bounded region. An
/// absent lexical cue is not evidence that a semantic result is irrelevant.
pub(crate) fn region_anchor(text: &str, range: LineRange, terms: &BTreeSet<String>) -> Option<u32> {
    let skip = usize::try_from(range.start().saturating_sub(1)).unwrap_or(usize::MAX);
    let count = usize::try_from(range.line_count().min(MAX_SCAN_LINES)).unwrap_or(usize::MAX);
    text.split('\n')
        .skip(skip)
        .take(count)
        .enumerate()
        .filter_map(|(offset, line)| {
            let shared = terms_of(line).intersection(terms).count();
            (shared > 0).then(|| {
                (
                    shared,
                    range
                        .start()
                        .saturating_add(u32::try_from(offset).unwrap_or(u32::MAX)),
                )
            })
        })
        .max_by(|(shared_a, line_a), (shared_b, line_b)| {
            shared_a.cmp(shared_b).then_with(|| line_b.cmp(line_a))
        })
        .map(|(_, line)| line)
}

/// Selects an actual source region, in 1-based inclusive lines. A complete
/// declaration is preferred when it fits. Larger declarations use a bounded
/// query-centered window; a retrieved span anchors ties and cue-free queries.
pub(crate) fn source_region(
    text: &str,
    retrieved: LineRange,
    declaration: Option<LineRange>,
    terms: &BTreeSet<String>,
    max_lines: u32,
) -> LineRange {
    let total = line_count(text).max(1);
    let retrieved = clamp(retrieved, total);
    let extent = declaration
        .filter(|range| range.start() <= retrieved.start() && retrieved.end() <= range.end())
        .map(|range| clamp(range, total))
        .unwrap_or(retrieved);
    let max_lines = max_lines.max(1);
    if extent.line_count() <= max_lines {
        return extent;
    }
    let anchor = retrieved.start().clamp(extent.start(), extent.end());
    let scan_start = anchor
        .saturating_sub(MAX_SCAN_LINES / 2)
        .max(extent.start());
    let scan_end = scan_start
        .saturating_add(MAX_SCAN_LINES.saturating_sub(1))
        .min(extent.end());
    let width = max_lines.min(scan_end.saturating_sub(scan_start).saturating_add(1));
    let mut window: VecDeque<BTreeSet<String>> = VecDeque::new();
    let mut counts: BTreeMap<String, u32> = BTreeMap::new();
    let mut best: Option<(usize, u32, u32, u32)> = None;
    let skip = usize::try_from(scan_start.saturating_sub(1)).unwrap_or(usize::MAX);
    let count = usize::try_from(scan_end.saturating_sub(scan_start).saturating_add(1))
        .unwrap_or(usize::MAX);
    for (offset, line) in text.split('\n').skip(skip).take(count).enumerate() {
        let cues: BTreeSet<_> = if terms.is_empty() {
            BTreeSet::new()
        } else {
            terms_of(line).intersection(terms).cloned().collect()
        };
        for cue in &cues {
            let frequency = counts.entry(cue.clone()).or_default();
            *frequency = frequency.saturating_add(1);
        }
        window.push_back(cues);
        if window.len() > usize::try_from(width).unwrap_or(usize::MAX)
            && let Some(expired) = window.pop_front()
        {
            for cue in expired {
                if let Some(frequency) = counts.get_mut(&cue) {
                    *frequency = frequency.saturating_sub(1);
                    if *frequency == 0 {
                        counts.remove(&cue);
                    }
                }
            }
        }
        if window.len() < usize::try_from(width).unwrap_or(usize::MAX) {
            continue;
        }
        let end = scan_start.saturating_add(u32::try_from(offset).unwrap_or(u32::MAX));
        let start = end.saturating_sub(width.saturating_sub(1));
        let distance = if anchor < start {
            start.saturating_sub(anchor)
        } else {
            anchor.saturating_sub(end)
        };
        let overlap_start = start.max(retrieved.start());
        let overlap_end = end.min(retrieved.end());
        let overlap = if overlap_start <= overlap_end {
            overlap_end.saturating_sub(overlap_start).saturating_add(1)
        } else {
            0
        };
        let score = (counts.len(), overlap, distance, start);
        let improves = best.is_none_or(
            |(shared, previous_overlap, previous_distance, previous_start)| {
                score.0 > shared
                    || (score.0 == shared
                        && (score.1 > previous_overlap
                            || (score.1 == previous_overlap
                                && (score.2, score.3) < (previous_distance, previous_start))))
            },
        );
        if improves {
            best = Some(score);
        }
    }
    let start = best.map_or(anchor, |(_, _, _, start)| start);
    let end = start
        .saturating_add(max_lines.saturating_sub(1))
        .min(extent.end());
    LineRange::new(start, end.max(start)).unwrap_or(retrieved)
}

fn clamp(range: LineRange, total: u32) -> LineRange {
    let start = range.start().min(total).max(1);
    let end = range.end().min(total).max(start);
    LineRange::new(start, end).unwrap_or(range)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_small_declaration_keeps_its_leading_guard_and_trailing_assertion() {
        let text = "fn parse() {\n    if missing_fixture() { return; }\n    decode();\n    assert!(bounded());\n}";
        let selected = source_region(
            text,
            LineRange::new(3, 3).unwrap(),
            Some(LineRange::new(1, 5).unwrap()),
            &region_terms("decode"),
            20,
        );
        assert_eq!(selected, LineRange::new(1, 5).unwrap());
    }

    #[test]
    fn a_late_matching_guard_survives_a_long_declaration() {
        let mut lines = vec!["fn decode() {".to_owned()];
        lines.extend((0..170).map(|_| "    advance();".to_owned()));
        lines.push("    if bytes > max_input_bytes { return Err(limit); }".to_owned());
        lines.push("}".to_owned());
        let text = lines.join("\n");
        let selected = source_region(
            &text,
            LineRange::new(1, 173).unwrap(),
            None,
            &region_terms("max_input_bytes limit"),
            24,
        );
        assert!(selected.start() > 40);
        assert!(selected.start() <= 172 && 172 <= selected.end());
        assert_eq!(selected.line_count(), 24);
    }

    #[test]
    fn cue_free_queries_keep_the_retrieved_region_instead_of_the_parent_prefix() {
        let text = (1..=200)
            .map(|n| format!("line {n}"))
            .collect::<Vec<_>>()
            .join("\n");
        let selected = source_region(
            &text,
            LineRange::new(150, 160).unwrap(),
            Some(LineRange::new(1, 200).unwrap()),
            &BTreeSet::new(),
            20,
        );
        assert!(selected.start() <= 150 && 150 <= selected.end());
        assert!(160 <= selected.end());
        assert!(selected.start() > 100);
    }

    #[test]
    fn unicode_and_trailing_newline_keep_real_line_coordinates() {
        let text = "başlık\nçizgi\n限界 max_vertices\nson\n";
        let selected = source_region(
            text,
            LineRange::new(1, 99).unwrap(),
            None,
            &region_terms("max_vertices"),
            2,
        );
        assert!(selected.start() <= 3 && 3 <= selected.end());
        assert!(selected.end() <= 4);
        assert_eq!(selected.line_count(), 2);
    }

    #[test]
    fn exact_source_slicing_preserves_crlf_and_a_final_blank_line() {
        let text = "fn read() {\r\n    decode();\r\n}\r\n\r\n";
        assert_eq!(
            slice_source_lines(text, LineRange::new(2, 3).unwrap()),
            "    decode();\r\n}\r\n"
        );
        assert_eq!(
            slice_source_lines(text, LineRange::new(4, 4).unwrap()),
            "\r\n"
        );
        assert_eq!(
            slice_source_lines("single", LineRange::new(1, 1).unwrap()),
            "single"
        );
    }

    #[test]
    fn zero_limit_is_safe_and_enclosing_ranges_choose_the_nearest_block() {
        let selected = source_region(
            "a\nb\nc",
            LineRange::new(2, 3).unwrap(),
            None,
            &BTreeSet::new(),
            0,
        );
        assert_eq!(selected.line_count(), 1);
        assert_eq!(
            enclosing_at(
                [
                    LineRange::new(1, 20).unwrap(),
                    LineRange::new(4, 8).unwrap()
                ]
                .into_iter(),
                5,
            ),
            Some(LineRange::new(4, 8).unwrap()),
        );
    }
}
