//! `know secrets …`

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::process::ExitCode;

use clap::Subcommand;
use knowell_secrets::{Exclusion, ExclusionPolicy};
use knowell_source::{SkipReason, WalkOptions};

use crate::output::Output;

#[derive(Debug, Subcommand)]
pub(crate) enum SecretsCommand {
    /// Walk a directory the way the indexer does and report which files are
    /// excluded before being read and which spans would be redacted.
    /// Secret values are never printed — only paths, lines and kinds.
    Scan {
        /// Directory to scan.
        #[arg(default_value = ".")]
        dir: PathBuf,
        /// Additional gitignore-style exclusion patterns.
        #[arg(long = "exclude", value_name = "GLOB")]
        exclude: Vec<String>,
        /// Exit with status 1 when any redaction finding exists in indexable
        /// content (useful as a pre-commit or CI check).
        #[arg(long)]
        strict: bool,
        /// Print the report as JSON.
        #[arg(long)]
        json: bool,
    },
}

pub(crate) fn run(cmd: SecretsCommand, out: &mut Output) -> anyhow::Result<ExitCode> {
    let SecretsCommand::Scan {
        dir,
        exclude,
        strict,
        json,
    } = cmd;
    let policy = ExclusionPolicy::with_patterns(&exclude)?;
    let report = knowell_source::walk(&dir, &policy, &WalkOptions::default())?;

    let findings: Vec<_> = report
        .files
        .iter()
        .flat_map(|file| file.redactions.iter().map(move |f| (&file.path, f)))
        .collect();
    let excluded: Vec<_> = report
        .skipped
        .iter()
        .filter_map(|s| match &s.reason {
            SkipReason::Excluded(exclusion) => Some((&s.path, exclusion)),
            _ => None,
        })
        .collect();

    if json {
        let value = serde_json::json!({
            "files_indexable": report.files.len(),
            "excluded": excluded.iter().map(|(path, exclusion)| serde_json::json!({
                "path": path,
                "exclusion": exclusion,
            })).collect::<Vec<_>>(),
            "findings": findings.iter().map(|(path, f)| serde_json::json!({
                "path": path,
                "line": f.line,
                "kind": f.kind,
            })).collect::<Vec<_>>(),
        });
        out.line(serde_json::to_string_pretty(&value)?)?;
    } else {
        out.line(format!(
            "{} indexable file(s), {} excluded by path, {} redaction finding(s)",
            report.files.len(),
            excluded.len(),
            findings.len()
        ))?;
        if !excluded.is_empty() {
            out.line("")?;
            out.line("excluded before reading:")?;
            for (path, exclusion) in &excluded {
                out.line(format!("  {path}  [{}]", describe(exclusion)))?;
            }
        }
        if !findings.is_empty() {
            out.line("")?;
            out.line("redacted before indexing:")?;
            for (path, finding) in &findings {
                out.line(format!(
                    "  {path}:{}  {}",
                    finding.line,
                    finding.kind.as_str()
                ))?;
            }
        }
        let other = skip_summary(&report.skipped);
        if !other.is_empty() {
            out.line("")?;
            out.line(format!("other skipped files: {other}"))?;
        }
    }
    out.flush()?;
    Ok(if strict && !findings.is_empty() {
        ExitCode::FAILURE
    } else {
        ExitCode::SUCCESS
    })
}

fn describe(exclusion: &Exclusion) -> String {
    match exclusion {
        Exclusion::Sensitive(kind) => format!("{}: {}", kind.as_str(), kind.reason()),
        Exclusion::Pattern(glob) => format!("pattern {glob}"),
        Exclusion::Internal => "internal".to_owned(),
    }
}

fn skip_summary(skipped: &[knowell_source::SkippedFile]) -> String {
    let mut counts: BTreeMap<&str, usize> = BTreeMap::new();
    for s in skipped {
        let label = match s.reason {
            SkipReason::Excluded(_) => continue,
            SkipReason::TooLarge { .. } => "too large",
            SkipReason::Binary => "binary",
            SkipReason::NotUtf8 => "not UTF-8",
            SkipReason::Unreadable { .. } => "unreadable",
            SkipReason::InvalidPath => "invalid path",
            SkipReason::Symlink => "symlink",
        };
        *counts.entry(label).or_default() += 1;
    }
    counts
        .iter()
        .map(|(label, n)| format!("{n} {label}"))
        .collect::<Vec<_>>()
        .join(", ")
}
