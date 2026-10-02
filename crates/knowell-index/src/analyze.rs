//! T1 analysis of one file version: parse, chunk, embedding inputs, symbol
//! rows and syntactic edges. Pure and blocking (runs on blocking threads).
//!
//! # Symbol identity
//!
//! `knowell-parse` names symbols by their container path inside the file
//! (`SubscriptionService.cancel`). The store identifies a logical symbol by
//! (project, kind, qualified name), so the engine qualifies parse names with
//! the file path: `src/billing/service.ts#SubscriptionService.cancel` (see
//! [`symbol_key`]). Two unrelated `main` functions in two files are thus two
//! symbols, and a renamed or moved file keeps its symbols' ids because the
//! pipeline renames them (`rename_symbol`) instead of creating new ones.

use std::collections::BTreeSet;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;

use knowell_core::{ContentHash, LineRange, RepoPath};
use knowell_graph::EdgeKind;
use knowell_parse::{
    ChunkContext, ChunkOptions, Degradation, ParseLimits, ParsedFile, PreparedInput, chunks,
    parse_with, prepared_input,
};
use knowell_store::content::NewChunk;
use knowell_store::graph::{NewEdge, NodeRef};
use knowell_store::symbols::NewSymbol;
use knowell_store::{EvidenceType, ProjectId, Resolution, SymbolId};

use crate::config::GeneratedPolicy;
use crate::references::{Identifier, identifiers};

/// Separator between the file path and the in-file qualified name.
pub const SYMBOL_PATH_SEPARATOR: char = '#';

/// The store's qualified name of a symbol: `<path>#<qualified name in file>`.
pub fn symbol_key(path: &RepoPath, local: &str) -> String {
    format!("{path}{SYMBOL_PATH_SEPARATOR}{local}")
}

/// Splits a store qualified name into the file path and the in-file name;
/// `None` when it was not produced by [`symbol_key`].
pub fn split_symbol_key(key: &str) -> Option<(RepoPath, &str)> {
    let (path, local) = key.split_once(SYMBOL_PATH_SEPARATOR)?;
    if local.is_empty() {
        return None;
    }
    Some((RepoPath::new(path).ok()?, local))
}

/// The parser version recorded with chunk rows (`p<PARSER_VERSION>`).
pub fn parser_version_tag() -> String {
    format!("p{}", knowell_parse::PARSER_VERSION)
}

/// Settings of one analysis.
#[derive(Debug, Clone, Copy)]
pub(crate) struct AnalyseOptions {
    pub(crate) chunking: ChunkOptions,
    pub(crate) limits: ParseLimits,
    pub(crate) generated: GeneratedPolicy,
    /// Whether to list identifier uses (a second parse; only T1 resolves
    /// references).
    pub(crate) identifiers: bool,
}

/// One chunk row with its embedding input.
#[derive(Debug, Clone)]
pub(crate) struct AnalysedChunk {
    pub(crate) row: NewChunk,
    pub(crate) input: PreparedInput,
}

/// Everything T1..T3 need from one file version.
#[derive(Debug, Clone)]
pub(crate) struct AnalysedFile {
    pub(crate) path: RepoPath,
    pub(crate) content_hash: ContentHash,
    pub(crate) text: Arc<str>,
    pub(crate) parsed: Arc<ParsedFile>,
    /// Chunks (empty for text-only files).
    pub(crate) chunks: Vec<AnalysedChunk>,
    /// Whether the content policy allows embedding this file.
    pub(crate) embed: bool,
    /// Whether symbols and edges are recorded (false for text-only files).
    pub(crate) structured: bool,
    /// Identifier uses (exact-tier languages only), for reference
    /// resolution (see [`crate::references`]).
    pub(crate) identifiers: Vec<Identifier>,
}

impl AnalysedFile {
    /// Approximate memory held, in bytes of text.
    pub(crate) fn weight(&self) -> usize {
        self.text.len()
            + self
                .chunks
                .iter()
                .map(|c| c.input.text.len() + c.input.title.len())
                .sum::<usize>()
            + self
                .identifiers
                .iter()
                .map(|i| i.name.len() + 8)
                .sum::<usize>()
    }
}

fn is_generated(parsed: &ParsedFile) -> bool {
    parsed.is_generated || matches!(parsed.degraded, Some(Degradation::Minified))
}

/// Parses and chunks one file version.
pub(crate) fn analyse(
    project: &str,
    path: &RepoPath,
    content_hash: ContentHash,
    text: Arc<str>,
    options: &AnalyseOptions,
    cancel: &AtomicBool,
) -> AnalysedFile {
    let parsed = parse_with(path, &text, &options.limits, Some(cancel));
    let generated = is_generated(&parsed);
    let structured = !(generated && options.generated == GeneratedPolicy::TextOnly);
    let embed = !generated || options.generated == GeneratedPolicy::Full;
    let parser_version = parser_version_tag();
    let mut out_chunks = Vec::new();
    if structured {
        match chunks(&parsed, &text, &options.chunking) {
            Ok(list) => {
                for chunk in &list {
                    let Ok(ordinal) = u32::try_from(chunk.ordinal) else {
                        continue;
                    };
                    let context = ChunkContext::for_chunk(project, &parsed, chunk);
                    let input = prepared_input(chunk, &context);
                    out_chunks.push(AnalysedChunk {
                        row: NewChunk {
                            content_hash,
                            parser_version: parser_version.clone(),
                            ordinal,
                            lines: chunk.range,
                            start_byte: chunk.byte_range.start as u64,
                            end_byte: chunk.byte_range.end as u64,
                            kind: chunk.kind.as_str().to_owned(),
                            symbol_path: chunk.symbol_path.clone(),
                            prepared_input_hash: input.hash,
                        },
                        input,
                    });
                }
            }
            Err(error) => {
                // Cannot happen for the text the file was parsed from; keep
                // the file (text and symbols) and report the gap.
                tracing::warn!(%path, %error, "chunking failed; the file has no chunks");
            }
        }
    }
    // A second, bounded parse lists identifier uses; knowell-parse reports
    // declarations and imports only.
    let identifiers = if options.identifiers && structured && parsed.degraded.is_none() {
        identifiers(parsed.language, &text, &options.limits)
    } else {
        Vec::new()
    };
    AnalysedFile {
        path: path.clone(),
        content_hash,
        text,
        parsed: Arc::new(parsed),
        chunks: out_chunks,
        embed,
        structured,
        identifiers,
    }
}

/// Symbol rows of a file, in parse order (index `i` is symbol `i`).
/// Symbols with an empty name get no row (`None`).
pub(crate) fn symbol_rows(file: &AnalysedFile) -> Vec<Option<NewSymbol>> {
    if !file.structured {
        return Vec::new();
    }
    file.parsed
        .symbols
        .iter()
        .map(|s| {
            (!s.qualified_name.is_empty()).then(|| NewSymbol {
                qualified_name: symbol_key(&file.path, &s.qualified_name),
                kind: s.kind.as_str().to_owned(),
            })
        })
        .collect()
}

fn evidence(path: &RepoPath, lines: Option<LineRange>, hash: &ContentHash) -> serde_json::Value {
    let mut value = serde_json::json!({
        "path": path.as_str(),
        "content_hash": hash.to_string(),
    });
    if let (Some(lines), Some(map)) = (lines, value.as_object_mut()) {
        map.insert(
            "lines".to_owned(),
            serde_json::json!([lines.start(), lines.end()]),
        );
    }
    value
}

/// Syntactic edges of a file: the file defines each of its symbols, a
/// container contains its members, and the file imports what it imports
/// (resolved to a file of the view when the specifier is a path, otherwise
/// an unresolved name). `ids[i]` is the id of symbol `i`.
pub(crate) fn syntactic_edges(
    project: ProjectId,
    file: &AnalysedFile,
    ids: &[Option<SymbolId>],
    files: &BTreeSet<RepoPath>,
) -> Vec<NewEdge> {
    if !file.structured {
        return Vec::new();
    }
    let origin = file.path.to_string();
    let file_node = NodeRef::File {
        project,
        path: file.path.clone(),
    };
    let mut edges = Vec::new();
    for (i, symbol) in file.parsed.symbols.iter().enumerate() {
        let Some(Some(id)) = ids.get(i) else { continue };
        let ev = evidence(&file.path, Some(symbol.range), &file.content_hash);
        edges.push(NewEdge {
            from: file_node.clone(),
            to: NodeRef::Symbol(*id),
            kind: EdgeKind::Defines.as_str().to_owned(),
            evidence_type: EvidenceType::Syntactic,
            resolution: Resolution::Resolved,
            evidence: ev.clone(),
            origin: origin.clone(),
        });
        if let Some(Some(parent)) = symbol.parent.and_then(|p| ids.get(p)) {
            edges.push(NewEdge {
                from: NodeRef::Symbol(*parent),
                to: NodeRef::Symbol(*id),
                kind: EdgeKind::Contains.as_str().to_owned(),
                evidence_type: EvidenceType::Syntactic,
                resolution: Resolution::Resolved,
                evidence: ev,
                origin: origin.clone(),
            });
        }
    }
    let mut seen = BTreeSet::new();
    for import in &file.parsed.imports {
        let specifier = import.specifier.trim();
        if specifier.is_empty() || !seen.insert(specifier.to_owned()) {
            continue;
        }
        let mut ev = evidence(&file.path, Some(import.range), &file.content_hash);
        if let Some(map) = ev.as_object_mut() {
            map.insert("specifier".to_owned(), serde_json::json!(specifier));
        }
        let (to, resolution) = match resolve_import(&file.path, specifier, files) {
            Some(target) => (
                NodeRef::File {
                    project,
                    path: target,
                },
                Resolution::Resolved,
            ),
            None => (
                NodeRef::Name {
                    project,
                    name: specifier.to_owned(),
                },
                Resolution::Unresolved,
            ),
        };
        edges.push(NewEdge {
            from: file_node.clone(),
            to,
            kind: EdgeKind::Imports.as_str().to_owned(),
            evidence_type: EvidenceType::Syntactic,
            resolution,
            evidence: ev,
            origin: origin.clone(),
        });
    }
    edges
}

/// The files of the view that `file`'s imports resolve to, in import order,
/// without duplicates.
pub(crate) fn import_targets(file: &AnalysedFile, files: &BTreeSet<RepoPath>) -> Vec<RepoPath> {
    let mut seen = BTreeSet::new();
    file.parsed
        .imports
        .iter()
        .filter_map(|import| resolve_import(&file.path, import.specifier.trim(), files))
        .filter(|target| target != &file.path && seen.insert(target.clone()))
        .collect()
}

/// File names that are imported by naming their directory.
const INDEX_FILES: &[&str] = &["index.ts", "index.tsx", "index.js", "__init__.py", "mod.rs"];

/// The last segments an import specifier may end with when it resolves to
/// `path` (see [`resolve_import`]): the file name, the name without one or
/// more extensions (`client.d.ts` → `client.d`, `client`), and the directory
/// name for index files (`api/index.ts` → `api`).
pub(crate) fn specifier_tails(path: &RepoPath) -> Vec<String> {
    let name = path.file_name();
    let mut tails = BTreeSet::new();
    tails.insert(name.to_owned());
    for (i, c) in name.char_indices() {
        if c == '.' && i > 0 {
            tails.insert(name.get(..i).unwrap_or_default().to_owned());
        }
    }
    if INDEX_FILES.contains(&name)
        && let Some(dir) = path.parent()
    {
        tails.insert(dir.file_name().to_owned());
    }
    tails.retain(|t| !t.is_empty());
    tails.into_iter().collect()
}

/// Extensions tried, in order, for an import specifier without one.
const IMPORT_EXTENSIONS: &[&str] = &[
    "ts", "tsx", "js", "jsx", "mjs", "cjs", "d.ts", "py", "dart", "rs", "go", "proto", "scss",
    "css",
];

/// Joins `relative` (which may contain `.` and `..`) onto `dir`; `None` when
/// it climbs above the root or is empty.
fn normalise_join(dir: Option<&RepoPath>, relative: &str) -> Option<RepoPath> {
    let mut parts: Vec<&str> = dir.map(|d| d.components().collect()).unwrap_or_default();
    for part in relative.split('/') {
        match part {
            "" | "." => {}
            ".." => {
                parts.pop()?;
            }
            other => parts.push(other),
        }
    }
    if parts.is_empty() {
        return None;
    }
    RepoPath::new(parts.join("/")).ok()
}

fn first_existing(
    stem: &RepoPath,
    files: &BTreeSet<RepoPath>,
    own_ext: Option<&str>,
) -> Option<RepoPath> {
    if files.contains(stem) {
        return Some(stem.clone());
    }
    let extensions = own_ext.into_iter().chain(IMPORT_EXTENSIONS.iter().copied());
    for ext in extensions {
        if let Ok(candidate) = RepoPath::new(format!("{stem}.{ext}"))
            && files.contains(&candidate)
        {
            return Some(candidate);
        }
    }
    for index in INDEX_FILES {
        if let Ok(candidate) = stem.join(index)
            && files.contains(&candidate)
        {
            return Some(candidate);
        }
    }
    None
}

/// Resolves an import specifier to a file of the view, syntactically:
/// relative specifiers (`./x`, `../y`) against the importing file's
/// directory; path-like specifiers (`util/strings.h`) against that directory
/// and then the project root. Package names stay unresolved.
pub(crate) fn resolve_import(
    from: &RepoPath,
    specifier: &str,
    files: &BTreeSet<RepoPath>,
) -> Option<RepoPath> {
    let dir = from.parent();
    let own_ext = from.extension();
    if specifier.starts_with("./") || specifier.starts_with("../") {
        let stem = normalise_join(dir.as_ref(), specifier)?;
        return first_existing(&stem, files, own_ext);
    }
    let path_like = specifier.contains('/')
        || specifier.rsplit_once('.').is_some_and(|(_, ext)| {
            ext.len() <= 5 && ext.chars().all(|c| c.is_ascii_alphanumeric())
        });
    if !path_like || specifier.starts_with('/') || specifier.contains("://") {
        return None;
    }
    if let Some(stem) = normalise_join(dir.as_ref(), specifier)
        && let Some(found) = first_existing(&stem, files, own_ext)
    {
        return Some(found);
    }
    let stem = normalise_join(None, specifier)?;
    if files.contains(&stem) {
        return Some(stem);
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(s: &str) -> RepoPath {
        RepoPath::new(s).unwrap()
    }

    fn files(list: &[&str]) -> BTreeSet<RepoPath> {
        list.iter().map(|s| p(s)).collect()
    }

    #[test]
    fn symbol_keys_round_trip() {
        let key = symbol_key(&p("src/a.ts"), "Service.cancel");
        assert_eq!(key, "src/a.ts#Service.cancel");
        let (path, local) = split_symbol_key(&key).unwrap();
        assert_eq!(path.as_str(), "src/a.ts");
        assert_eq!(local, "Service.cancel");
        assert!(split_symbol_key("no-separator").is_none());
        assert!(split_symbol_key("a.ts#").is_none());
        // A '#' inside the local name stays in the local part.
        let (_, local) = split_symbol_key("a.css#a #b").unwrap();
        assert_eq!(local, "a #b");
        assert_eq!(
            parser_version_tag(),
            format!("p{}", knowell_parse::PARSER_VERSION)
        );
    }

    #[test]
    fn resolves_relative_and_path_like_imports() {
        let set = files(&[
            "src/api/client.ts",
            "src/api/index.ts",
            "src/util.ts",
            "include/strings.h",
            "pkg/mod.rs",
        ]);
        let from = p("src/app/main.ts");
        assert_eq!(
            resolve_import(&from, "../api/client", &set),
            Some(p("src/api/client.ts"))
        );
        assert_eq!(
            resolve_import(&from, "../api", &set),
            Some(p("src/api/index.ts"))
        );
        assert_eq!(
            resolve_import(&from, "../util.ts", &set),
            Some(p("src/util.ts"))
        );
        assert_eq!(resolve_import(&from, "../../../x", &set), None);
        assert_eq!(resolve_import(&from, "react", &set), None);
        assert_eq!(resolve_import(&from, "@scope/pkg", &set), None);
        let c = p("src/x.c");
        assert_eq!(
            resolve_import(&c, "include/strings.h", &set),
            Some(p("include/strings.h"))
        );
        assert_eq!(resolve_import(&c, "/etc/passwd", &set), None);
        assert_eq!(resolve_import(&c, "https://x/y.js", &set), None);
        assert_eq!(
            resolve_import(&p("lib.rs"), "./pkg", &set),
            Some(p("pkg/mod.rs"))
        );
    }

    #[test]
    fn specifier_tails_cover_names_stems_and_index_directories() {
        assert_eq!(
            specifier_tails(&p("src/api/client.d.ts")),
            ["client", "client.d", "client.d.ts"]
        );
        assert_eq!(
            specifier_tails(&p("src/api/index.ts")),
            ["api", "index", "index.ts"]
        );
        assert_eq!(specifier_tails(&p("Makefile")), ["Makefile"]);
        assert_eq!(specifier_tails(&p(".eslintrc")), [".eslintrc"]);
        // Every specifier that resolves to the file ends with one of them.
        let set = files(&["src/api/client.ts", "src/api/index.ts", "lib/mod.rs"]);
        for (from, specifier, target) in [
            ("src/app/main.ts", "../api/client", "src/api/client.ts"),
            ("src/app/main.ts", "../api/client.ts", "src/api/client.ts"),
            ("src/app/main.ts", "../api", "src/api/index.ts"),
            ("main.rs", "./lib", "lib/mod.rs"),
        ] {
            assert_eq!(
                resolve_import(&p(from), specifier, &set),
                Some(p(target)),
                "{specifier}"
            );
            let tail = specifier.rsplit('/').next().unwrap();
            assert!(
                specifier_tails(&p(target)).iter().any(|t| t == tail),
                "{specifier}"
            );
        }
    }

    #[test]
    fn analyses_symbols_chunks_and_edges() {
        let text: Arc<str> = Arc::from(
            "import { helper } from './util';\n\nexport class Service {\n  cancel(id: string) { return helper(id); }\n}\n",
        );
        let path = p("src/service.ts");
        let hash = ContentHash::of(text.as_bytes());
        let options = AnalyseOptions {
            chunking: ChunkOptions::default(),
            limits: ParseLimits::default(),
            generated: GeneratedPolicy::SkipEmbeddings,
            identifiers: true,
        };
        let file = analyse(
            "billing",
            &path,
            hash,
            text,
            &options,
            &AtomicBool::new(false),
        );
        assert!(file.structured && file.embed);
        assert!(!file.chunks.is_empty());
        assert!(file.chunks.iter().all(|c| c.row.content_hash == hash));
        let rows = symbol_rows(&file);
        let names: Vec<String> = rows
            .iter()
            .flatten()
            .map(|r| r.qualified_name.clone())
            .collect();
        assert!(
            names.contains(&"src/service.ts#Service".to_owned()),
            "{names:?}"
        );
        assert!(
            names.contains(&"src/service.ts#Service.cancel".to_owned()),
            "{names:?}"
        );

        let ids: Vec<Option<SymbolId>> = rows
            .iter()
            .enumerate()
            .map(|(i, _)| Some(SymbolId(uuid::Uuid::from_u128(i as u128 + 1))))
            .collect();
        let project = ProjectId(uuid::Uuid::nil());
        let edges = syntactic_edges(
            project,
            &file,
            &ids,
            &files(&["src/util.ts", "src/service.ts"]),
        );
        assert!(edges.iter().all(|e| e.origin == "src/service.ts"));
        assert!(edges.iter().any(|e| e.kind == "defines"));
        assert!(edges.iter().any(|e| e.kind == "contains"));
        let import = edges.iter().find(|e| e.kind == "imports").unwrap();
        assert_eq!(import.resolution, Resolution::Resolved);
        assert_eq!(
            import.to,
            NodeRef::File {
                project,
                path: p("src/util.ts")
            }
        );
    }

    #[test]
    fn generated_files_follow_the_policy() {
        let text: Arc<str> =
            Arc::from("// Code generated by protoc. DO NOT EDIT.\npackage x\n\nfunc A() {}\n");
        let path = p("api/x.pb.go");
        let hash = ContentHash::of(text.as_bytes());
        let mut options = AnalyseOptions {
            chunking: ChunkOptions::default(),
            limits: ParseLimits::default(),
            generated: GeneratedPolicy::SkipEmbeddings,
            identifiers: false,
        };
        let cancel = AtomicBool::new(false);
        let skip = analyse("p", &path, hash, text.clone(), &options, &cancel);
        assert!(skip.structured && !skip.embed);
        options.generated = GeneratedPolicy::TextOnly;
        let text_only = analyse("p", &path, hash, text.clone(), &options, &cancel);
        assert!(!text_only.structured && text_only.chunks.is_empty());
        assert!(symbol_rows(&text_only).is_empty());
        options.generated = GeneratedPolicy::Full;
        let full = analyse("p", &path, hash, text, &options, &cancel);
        assert!(full.embed);
    }
}
