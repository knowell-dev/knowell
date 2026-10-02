//! Dockerfile build stages and base images (hand-written scanner: there is
//! no bundled Dockerfile grammar).

use std::ops::Range;

use crate::extract::{Draft, clean_doc};
use crate::language::Language;
use crate::model::SymbolKind;
use crate::text::{bounded, one_line, trim_end_offset};

const MAX_SIGNATURE_BYTES: usize = 300;

/// One logical instruction (continuation lines joined).
struct Instruction {
    keyword: String,
    args: String,
    range: Range<usize>,
    /// Start of the directly preceding comment lines, if any.
    lead_start: usize,
    comments: Vec<String>,
}

pub(crate) struct Scan {
    pub(crate) drafts: Vec<Draft>,
    /// `(statement byte range, image)` for `FROM` and `COPY --from` images.
    pub(crate) imports: Vec<(Range<usize>, String)>,
}

pub(crate) fn scan(text: &str, max_symbols: usize) -> Scan {
    let instructions = instructions(text);
    let mut aliases: Vec<String> = Vec::new();
    let mut imports = Vec::new();
    let mut stages: Vec<(usize, &Instruction, String)> = Vec::new();
    for instruction in &instructions {
        match instruction.keyword.as_str() {
            "FROM" => {
                let mut words = instruction
                    .args
                    .split_whitespace()
                    .filter(|w| !w.starts_with("--"));
                let image = words.next().unwrap_or("").to_owned();
                let alias = match (words.next(), words.next()) {
                    (Some(kw), Some(alias)) if kw.eq_ignore_ascii_case("as") => {
                        Some(alias.to_owned())
                    }
                    _ => None,
                };
                let is_stage_ref = aliases.iter().any(|a| a.eq_ignore_ascii_case(&image));
                if !image.is_empty() && !is_stage_ref && image != "scratch" {
                    imports.push((instruction.range.clone(), image.clone()));
                }
                let name = alias.clone().unwrap_or_else(|| image.clone());
                if let Some(alias) = alias {
                    aliases.push(alias);
                }
                if !name.is_empty() && stages.len() < max_symbols {
                    stages.push((instruction.lead_start, instruction, name));
                }
            }
            "COPY" | "ADD" => {
                let from = instruction
                    .args
                    .split_whitespace()
                    .find_map(|w| w.strip_prefix("--from="));
                if let Some(source) = from {
                    let is_stage = aliases.iter().any(|a| a.eq_ignore_ascii_case(source))
                        || source.chars().all(|c| c.is_ascii_digit());
                    if !is_stage {
                        imports.push((instruction.range.clone(), source.to_owned()));
                    }
                }
            }
            _ => {}
        }
    }
    let starts: Vec<usize> = stages.iter().map(|(start, _, _)| *start).collect();
    let drafts = stages
        .iter()
        .enumerate()
        .map(|(index, (start, from, name))| {
            let end = starts.get(index + 1).copied().unwrap_or(text.len());
            let end = trim_end_offset(text, *start, end);
            let signature = bounded(
                &one_line(&format!("{} {}", from.keyword, from.args)),
                MAX_SIGNATURE_BYTES,
            );
            let mut draft = Draft::simple(SymbolKind::Stage, name.clone(), *start..end, signature);
            draft.decl_start = from.range.start;
            draft.name_start = from.range.start;
            let comments: Vec<&str> = from.comments.iter().map(String::as_str).collect();
            draft.doc = clean_doc(&comments, Language::Dockerfile);
            draft
        })
        .collect();
    Scan { drafts, imports }
}

fn instructions(text: &str) -> Vec<Instruction> {
    let escape = escape_char(text);
    let mut out = Vec::new();
    let mut comments: Vec<String> = Vec::new();
    let mut comment_start: Option<usize> = None;
    let mut current: Option<(usize, String)> = None;
    let mut offset = 0;
    for line in text.split_inclusive('\n') {
        let start = offset;
        offset += line.len();
        let content = line.trim();
        if let Some((begin, mut logical)) = current.take() {
            // Inside a continuation: comment lines are skipped.
            if content.starts_with('#') {
                current = Some((begin, logical));
                continue;
            }
            let (piece, continues) = strip_continuation(content, escape);
            logical.push(' ');
            logical.push_str(piece);
            if continues {
                current = Some((begin, logical));
            } else {
                push_instruction(
                    &mut out,
                    text,
                    begin,
                    offset,
                    &logical,
                    &mut comments,
                    &mut comment_start,
                );
            }
            continue;
        }
        if content.is_empty() {
            comments.clear();
            comment_start = None;
            continue;
        }
        if content.starts_with('#') {
            if out.is_empty() && is_parser_directive(content) {
                continue;
            }
            comment_start.get_or_insert(start);
            comments.push(content.to_owned());
            continue;
        }
        let (piece, continues) = strip_continuation(content, escape);
        if continues {
            current = Some((start, piece.to_owned()));
        } else {
            push_instruction(
                &mut out,
                text,
                start,
                offset,
                piece,
                &mut comments,
                &mut comment_start,
            );
        }
    }
    if let Some((begin, logical)) = current {
        push_instruction(
            &mut out,
            text,
            begin,
            text.len(),
            &logical,
            &mut comments,
            &mut comment_start,
        );
    }
    out
}

fn push_instruction(
    out: &mut Vec<Instruction>,
    text: &str,
    start: usize,
    end: usize,
    logical: &str,
    comments: &mut Vec<String>,
    comment_start: &mut Option<usize>,
) {
    let logical = logical.trim();
    let (keyword, args) = logical
        .split_once(char::is_whitespace)
        .unwrap_or((logical, ""));
    out.push(Instruction {
        keyword: keyword.to_ascii_uppercase(),
        args: args.trim().to_owned(),
        range: start..trim_end_offset(text, start, end),
        lead_start: comment_start.take().unwrap_or(start),
        comments: std::mem::take(comments),
    });
}

fn strip_continuation(line: &str, escape: char) -> (&str, bool) {
    match line.strip_suffix(escape) {
        Some(rest) => (rest.trim_end(), true),
        None => (line, false),
    }
}

/// `# syntax=…`, `# escape=…`, `# check=…` at the top of the file.
fn is_parser_directive(line: &str) -> bool {
    let Some(rest) = line.strip_prefix('#') else {
        return false;
    };
    let Some((key, _)) = rest.split_once('=') else {
        return false;
    };
    matches!(
        key.trim().to_ascii_lowercase().as_str(),
        "syntax" | "escape" | "check"
    )
}

/// The `# escape=` parser directive (default `\`).
fn escape_char(text: &str) -> char {
    for line in text.lines() {
        let line = line.trim();
        let Some(directive) = line.strip_prefix('#') else {
            break;
        };
        if let Some(value) = directive
            .trim()
            .strip_prefix("escape")
            .and_then(|rest| rest.trim_start().strip_prefix('='))
        {
            return value.trim().chars().next().unwrap_or('\\');
        }
    }
    '\\'
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stages_and_images() {
        let text = "# syntax=docker/dockerfile:1\nARG VERSION=1\n\n# Build stage.\nFROM --platform=$BUILDPLATFORM rust:1.80 AS build\nRUN cargo build \\\n    --release\n\nFROM gcr.io/distroless/cc\nCOPY --from=build /app /app\nCOPY --from=nginx:latest /etc/nginx /etc/nginx\n";
        let scan = scan(text, 100);
        let names: Vec<&str> = scan.drafts.iter().map(|d| d.name.as_str()).collect();
        assert_eq!(names, ["build", "gcr.io/distroless/cc"]);
        let images: Vec<&str> = scan.imports.iter().map(|(_, i)| i.as_str()).collect();
        assert_eq!(
            images,
            ["rust:1.80", "gcr.io/distroless/cc", "nginx:latest"]
        );
        let build = &scan.drafts[0];
        assert_eq!(build.doc.as_deref(), Some("Build stage."));
        assert_eq!(
            build.signature.as_deref(),
            Some("FROM --platform=$BUILDPLATFORM rust:1.80 AS build")
        );
        assert!(text[build.range.clone()].starts_with("# Build stage."));
        assert!(text[build.range.clone()].ends_with("--release"));
    }

    #[test]
    fn escape_directive_and_unterminated_continuation() {
        let text = "# escape=`\nFROM mcr.microsoft.com/windows AS base\nRUN dir `\n";
        let scan = scan(text, 100);
        assert_eq!(scan.drafts.len(), 1);
        assert_eq!(escape_char(text), '`');
        assert!(instructions("RUN a \\").len() == 1);
    }
}
