//! Safe text and Markdown output shared by the local graph commands.

use std::path::PathBuf;

use clap::{Args, ValueEnum};
use knowell_mcp::Evidence;

use crate::{fsutil, local_engine, output::Output};

/// Output encoding for sourced graph reports.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, ValueEnum)]
pub(crate) enum GraphFormat {
    #[default]
    Text,
    Json,
    Markdown,
}

/// Output selection; files are written only when the caller passes `--output`.
#[derive(Debug, Args)]
pub(crate) struct GraphOutputArgs {
    /// Report encoding.
    #[arg(long, value_enum, conflicts_with = "json")]
    format: Option<GraphFormat>,
    /// Print the structured report as JSON.
    #[arg(long, conflicts_with = "format")]
    json: bool,
    /// Write the report atomically to this file instead of stdout.
    #[arg(long, value_name = "FILE")]
    output: Option<PathBuf>,
}

impl GraphOutputArgs {
    /// Selects JSON shorthand or the requested format; text is the default.
    pub(crate) fn format(&self) -> GraphFormat {
        if self.json {
            GraphFormat::Json
        } else {
            self.format.unwrap_or_default()
        }
    }

    /// Writes one complete report after computation has succeeded.
    pub(crate) fn emit(&self, body: &str, out: &mut Output) -> anyhow::Result<()> {
        if let Some(path) = &self.output {
            let terminated = if body.ends_with('\n') {
                body.to_owned()
            } else {
                format!("{body}\n")
            };
            fsutil::write_atomic(path, &terminated)
        } else {
            out.line(body)?;
            out.flush()?;
            Ok(())
        }
    }
}

/// Escapes an untrusted label as one Markdown paragraph or table cell.
pub(crate) fn markdown_text(text: &str) -> String {
    let text = local_engine::terminal_text(text);
    let mut escaped = String::with_capacity(text.len());
    for character in text.chars() {
        if matches!(
            character,
            '\\' | '`' | '*' | '_' | '[' | ']' | '(' | ')' | '<' | '>' | '#' | '!' | '|' | '~'
        ) {
            escaped.push('\\');
        }
        escaped.push(character);
    }
    escaped
}

/// Formats 1-based inclusive source lines and the pinned version metadata.
pub(crate) fn evidence_text(evidence: &Evidence) -> String {
    format!(
        "{}:{}:{}-{}; view {}; layer {}; commit {}; hash {}; freshness {}; index {}",
        evidence.project,
        local_engine::terminal_text(evidence.path.as_str()),
        evidence.lines.start(),
        evidence.lines.end(),
        local_engine::terminal_text(&evidence.view.to_string()),
        match evidence.layer {
            knowell_mcp::ViewLayer::Shared => "shared",
            knowell_mcp::ViewLayer::Personal => "personal",
        },
        evidence.commit,
        evidence.content_hash,
        evidence.freshness.as_str(),
        evidence.index_state.as_str(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn source_labels_cannot_create_markdown_links_html_or_code_fences() {
        let source = "![canary](https://example.invalid) <script> ```\n# title\u{001b}";
        let escaped = markdown_text(source);
        assert!(escaped.contains("\\!\\[canary\\]\\(https://example.invalid\\)"));
        assert!(escaped.contains("\\<script\\>"));
        assert!(escaped.contains("\\`\\`\\`"));
        assert!(!escaped.chars().any(char::is_control));
    }
}
