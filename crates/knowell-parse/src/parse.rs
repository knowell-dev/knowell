//! The parse driver: bounds, tree-sitter invocation and dispatch to the
//! extractors.

use std::ops::ControlFlow;
use std::sync::atomic::AtomicBool;
use std::time::Instant;

use knowell_core::{ContentHash, RepoPath};
use tree_sitter::{ParseOptions, ParseState, Parser, Tree};

use crate::extract::{self, Budget, Draft};
use crate::grammar::{self, Grammar};
use crate::language::{Language, Tier};
use crate::model::{Degradation, Import, ParseLimits, ParsedFile};
use crate::text::{LineIndex, bounded};
use crate::{dockerfile, generated, markdown, sql, structured};

/// Analyses one file with the default [`ParseLimits`].
///
/// Never fails and never panics: unsupported languages yield a text-only
/// result, oversized / minified / pathological input is recorded in
/// [`ParsedFile::degraded`], and syntax errors are tolerated (tree-sitter
/// error recovery; [`ParsedFile::has_errors`] is set).
pub fn parse(path: &RepoPath, text: &str) -> ParsedFile {
    parse_with(path, text, &ParseLimits::default(), None)
}

/// Analyses one file with explicit limits. Setting `cancel` to `true` from
/// another thread stops parsing at the next progress check; the result is
/// then marked [`Degradation::Cancelled`].
pub fn parse_with(
    path: &RepoPath,
    text: &str,
    limits: &ParseLimits,
    cancel: Option<&AtomicBool>,
) -> ParsedFile {
    let language = Language::detect(path, text);
    let lines = LineIndex::new(text);
    let mut file = ParsedFile {
        path: path.clone(),
        language,
        tier: language.tier(),
        dialect: None,
        symbols: Vec::new(),
        imports: Vec::new(),
        blocks: Vec::new(),
        is_generated: generated::is_generated(path, text),
        has_errors: false,
        degraded: None,
        content_hash: ContentHash::of(text.as_bytes()),
        byte_len: text.len(),
        line_count: lines.line_count(),
    };
    if text.len() > limits.max_bytes {
        file.degraded = Some(Degradation::TooLarge {
            bytes: text.len(),
            limit: limits.max_bytes,
        });
        return file;
    }
    if file.tier == Tier::TextOnly || text.trim().is_empty() {
        return file;
    }
    if let Some(reason) = rejects(text, language, limits) {
        file.degraded = Some(reason);
        return file;
    }
    let budget = Budget {
        deadline: Instant::now() + limits.timeout,
        cancel,
    };
    if language == Language::Dockerfile {
        let scan = dockerfile::scan(text, limits.max_symbols);
        file.symbols = extract::finish(scan.drafts, text, &lines, language);
        file.imports = scan
            .imports
            .into_iter()
            .filter_map(|(range, specifier)| {
                Some(Import {
                    specifier: bounded(&specifier, 512),
                    range: lines.range(&range)?,
                })
            })
            .collect();
        return file;
    }
    let Some(grammar) = Grammar::for_language(language) else {
        return file;
    };
    let compiled = match grammar::compiled(grammar) {
        Ok(compiled) => compiled,
        Err(message) => {
            file.degraded = Some(Degradation::GrammarError { message });
            return file;
        }
    };
    let tree = match run_parser(&compiled.language, text, budget) {
        Ok(tree) => tree,
        Err(reason) => {
            file.degraded = Some(reason);
            return file;
        }
    };
    file.has_errors = tree.root_node().has_error();

    let mut degraded: Option<Degradation> = None;
    let drafts: Vec<Draft> = match language {
        Language::Yaml | Language::Json | Language::Toml => {
            let found =
                structured::extract(&tree, text, language, path, budget, limits.max_symbols);
            file.dialect = found.dialect;
            degraded = found.degraded;
            found.drafts
        }
        Language::Markdown => match &compiled.symbols {
            Some(query) => {
                let (drafts, reason) =
                    markdown::headings(&tree, text, query, budget, limits.max_symbols);
                degraded = reason;
                drafts
            }
            None => Vec::new(),
        },
        _ => match &compiled.symbols {
            Some(query) => {
                let (drafts, reason) = extract::query_symbols(
                    &tree,
                    text,
                    query,
                    language,
                    budget,
                    limits.max_symbols,
                );
                degraded = reason;
                drafts
            }
            None => Vec::new(),
        },
    };
    let mut drafts = drafts;
    if language == Language::CSharp || language == Language::Php {
        extend_file_scoped_namespaces(&mut drafts, text);
    }
    file.symbols = extract::finish(drafts, text, &lines, language);
    if file.symbols.len() > limits.max_symbols {
        file.symbols.truncate(limits.max_symbols);
        degraded.get_or_insert(Degradation::Truncated {
            limit: limits.max_symbols,
        });
    }
    if language == Language::Sql {
        file.blocks = sql::blocks(&tree, text, &lines, limits.max_symbols);
    }
    // Imports are still extracted after symbol truncation, but not once the
    // time budget is spent.
    let out_of_time = matches!(
        degraded,
        Some(Degradation::Timeout | Degradation::Cancelled)
    );
    if !out_of_time && let Some(query) = &compiled.imports {
        let (imports, reason) = extract::query_imports(&tree, text, query, &lines, budget);
        file.imports = imports;
        if degraded.is_none() {
            degraded = reason;
        }
    }
    file.degraded = degraded;
    file
}

/// The tree-sitter grammar Knowell uses for `language` (YAML / JSON for
/// OpenAPI, AsyncAPI, Compose and Kubernetes files), or `None` for
/// text-only languages and the Dockerfile, which has no bundled grammar.
///
/// For dependents that run their own queries (contract rule packs); use the
/// re-exported [`tree_sitter`](crate::tree_sitter) crate so the versions
/// match.
pub fn ts_language(language: Language) -> Option<tree_sitter::Language> {
    Grammar::for_language(language).map(Grammar::ts_language)
}

/// Parses `text` as `language` under the same bounds as [`parse_with`]:
/// `None` when the language has no grammar, the text is empty or exceeds
/// `limits.max_bytes`, looks minified, nests deeper than
/// `limits.max_nesting`, or parsing exceeds `limits.timeout`. The tree may
/// contain `ERROR` nodes. Call [`parse_with`] when the reason matters: it
/// reports it in [`ParsedFile::degraded`].
pub fn parse_tree(language: Language, text: &str, limits: &ParseLimits) -> Option<Tree> {
    if text.len() > limits.max_bytes || text.trim().is_empty() {
        return None;
    }
    if rejects(text, language, limits).is_some() {
        return None;
    }
    let ts_language = ts_language(language)?;
    let budget = Budget {
        deadline: Instant::now() + limits.timeout,
        cancel: None,
    };
    run_parser(&ts_language, text, budget).ok()
}

/// Content checks made before tree-sitter runs.
fn rejects(text: &str, language: Language, limits: &ParseLimits) -> Option<Degradation> {
    if checks_minified(language) && looks_minified(text) {
        return Some(Degradation::Minified);
    }
    let depth = nesting_depth(text, language, limits.max_nesting);
    (depth > limits.max_nesting).then_some(Degradation::TooDeep {
        depth,
        limit: limits.max_nesting,
    })
}

fn run_parser(
    language: &tree_sitter::Language,
    text: &str,
    budget: Budget<'_>,
) -> Result<Tree, Degradation> {
    let mut parser = Parser::new();
    parser
        .set_language(language)
        .map_err(|e| Degradation::GrammarError {
            message: e.to_string(),
        })?;
    let bytes = text.as_bytes();
    let mut progress = |_: &ParseState| {
        if budget.exceeded().is_some() {
            ControlFlow::Break(())
        } else {
            ControlFlow::Continue(())
        }
    };
    let options = ParseOptions::new().progress_callback(&mut progress);
    let tree = parser.parse_with_options(
        &mut |offset, _| bytes.get(offset..).unwrap_or_default(),
        None,
        Some(options),
    );
    tree.ok_or_else(|| budget.exceeded().unwrap_or(Degradation::Timeout))
}

/// File-scoped namespaces (`namespace A.B;`) enclose everything after them,
/// up to the next file-scoped namespace.
fn extend_file_scoped_namespaces(drafts: &mut [Draft], text: &str) {
    let mut starts: Vec<usize> = drafts
        .iter()
        .filter(|d| is_file_scoped_namespace(d, text))
        .map(|d| d.range.start)
        .collect();
    starts.sort_unstable();
    for draft in drafts.iter_mut() {
        if is_file_scoped_namespace(draft, text) {
            let after = starts.partition_point(|&s| s <= draft.range.start);
            let next = starts.get(after).copied().unwrap_or(text.len());
            draft.range.end = draft.range.end.max(next);
        }
    }
}

fn is_file_scoped_namespace(draft: &Draft, text: &str) -> bool {
    draft.kind == crate::SymbolKind::Module
        && draft.body_start.is_none()
        && crate::text::slice(text, &(draft.decl_start..draft.range.end))
            .trim_end()
            .ends_with(';')
}

/// Languages where a single huge line means generated output.
fn checks_minified(language: Language) -> bool {
    !matches!(
        language,
        Language::Markdown | Language::Yaml | Language::Toml | Language::Dockerfile
    )
}

/// Minified bundles: long lines on average and at least one very long line.
pub(crate) fn looks_minified(text: &str) -> bool {
    if text.len() < 4096 {
        return false;
    }
    let mut lines = 0usize;
    let mut longest = 0usize;
    for line in text.split('\n') {
        lines += 1;
        longest = longest.max(line.len());
    }
    longest >= 2000 && text.len() / lines.max(1) >= 300
}

/// Approximate nesting depth: bracket depth (ignoring strings) for code,
/// and indentation width / 8 for every language. Saturates just above
/// `limit` so the scan stays cheap.
fn nesting_depth(text: &str, language: Language, limit: usize) -> usize {
    let brackets = !matches!(language, Language::Markdown | Language::Dockerfile);
    let mut depth = 0usize;
    let mut max = 0usize;
    let mut indent = 0usize;
    let mut at_line_start = true;
    for byte in text.bytes() {
        match byte {
            b'\n' => {
                at_line_start = true;
                indent = 0;
                continue;
            }
            b' ' if at_line_start => indent += 1,
            b'\t' if at_line_start => indent += 4,
            _ => {
                at_line_start = false;
            }
        }
        if indent / 8 > max {
            max = indent / 8;
        }
        if brackets {
            match byte {
                b'(' | b'[' | b'{' => {
                    depth += 1;
                    max = max.max(depth);
                }
                b')' | b']' | b'}' => depth = depth.saturating_sub(1),
                _ => {}
            }
        }
        if max > limit {
            return max;
        }
    }
    max
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nesting() {
        assert_eq!(
            nesting_depth("f(a[1], {b: (c)})", Language::JavaScript, 10),
            3
        );
        assert_eq!(nesting_depth("))))((", Language::JavaScript, 10), 2);
        let deep = "(".repeat(5000);
        assert_eq!(nesting_depth(&deep, Language::JavaScript, 100), 101);
        assert_eq!(nesting_depth(&deep, Language::Markdown, 100), 0);
        let indented = format!("{}x\n", " ".repeat(80));
        assert_eq!(nesting_depth(&indented, Language::Python, 100), 10);
    }

    #[test]
    fn minified() {
        let bundle = "var a=1;".repeat(1000);
        assert!(looks_minified(&bundle));
        let code = "let a = 1;\n".repeat(1000);
        assert!(!looks_minified(&code));
        assert!(!looks_minified("x"));
    }
}
