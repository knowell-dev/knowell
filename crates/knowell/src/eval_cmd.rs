//! `know eval …`

use std::fs::OpenOptions;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use anyhow::{Context, bail};
use clap::{Subcommand, ValueEnum};
use knowell_eval::{
    Bm25Retriever, Fixture, FixtureSpec, GrepRetriever, QuerySet, Report, Retriever, Scale,
    WriteOptions, generate, walk_fixture,
};

use crate::output::Output;

#[derive(Debug, Subcommand)]
pub(crate) enum EvalCommand {
    /// Write the synthetic multi-project evaluation workspace to a directory.
    Generate {
        /// Output directory (must not exist or be empty).
        #[arg(long)]
        out: PathBuf,
        /// Seed of the deterministic generator.
        #[arg(long, default_value_t = 42)]
        seed: u64,
        /// Workspace size.
        #[arg(long, value_enum, default_value_t = ScaleArg::Small)]
        scale: ScaleArg,
        /// Make every project its own git repository with one deterministic commit.
        #[arg(long)]
        git: bool,
    },
    /// Generate the workspace, index it the way Knowell does (secret boundary
    /// included) and measure retrievers on the built-in graded query set.
    Run {
        /// Seed of the deterministic generator.
        #[arg(long, default_value_t = 42)]
        seed: u64,
        /// Workspace size.
        #[arg(long, value_enum, default_value_t = ScaleArg::Small)]
        scale: ScaleArg,
        /// Retrievers to measure (repeatable). Default: grep and bm25.
        #[arg(long = "retriever", value_enum)]
        retrievers: Vec<RetrieverArg>,
        /// Results requested per query.
        #[arg(long, default_value_t = 10)]
        depth: usize,
        /// Write the report as JSON (this is also the baseline format).
        #[arg(long, value_name = "FILE")]
        json: Option<PathBuf>,
        /// Append the Markdown report to this file (e.g. `$GITHUB_STEP_SUMMARY`)
        /// instead of printing it.
        #[arg(long, value_name = "FILE")]
        markdown: Option<PathBuf>,
        /// Compare with a committed baseline report; exit 1 on any regression.
        #[arg(long, value_name = "FILE")]
        baseline: Option<PathBuf>,
        /// Largest metric drop that is not a regression.
        #[arg(long, default_value_t = 1e-4)]
        tolerance: f64,
        /// Keep the generated workspace in this directory instead of a
        /// temporary one.
        #[arg(long, value_name = "DIR")]
        keep: Option<PathBuf>,
        /// Override the BM25 coordination exponent (tuning experiments).
        #[arg(long, value_name = "EXPONENT")]
        bm25_coordination: Option<f32>,
        /// Admin PostgreSQL URL reference (env:NAME or file:/path). Hybrid
        /// creates and drops its own scratch database; pgvector is required.
        #[arg(long, value_name = "SECRET_REF")]
        database_url: Option<String>,
    },
    /// Send only the built-in Small synthetic fixture to Gemini Embedding 2
    /// and measure grep, BM25 and hybrid. May incur provider charges.
    Live {
        /// PostgreSQL admin URL reference; creates a disposable database.
        #[arg(long, value_name = "SECRET_REF")]
        database_url: String,
        /// Provider API key reference (env:NAME or file:/path), never a value.
        #[arg(long, value_name = "SECRET_REF")]
        api_key_ref: String,
        /// Requested Gemini dimensions: 768, 1536 or 3072.
        #[arg(long)]
        dimensions: u32,
        /// Input-token budget shared by indexing and queries (1..=500000).
        /// Reservations are estimates; this is not a provider billing limit.
        #[arg(long, default_value_t = 500_000)]
        max_tokens: u64,
        /// Report with model, dimensions, usage and measurement conditions.
        #[arg(long, value_name = "FILE")]
        json: PathBuf,
        /// Append the Markdown report to this file instead of stdout.
        #[arg(long, value_name = "FILE")]
        markdown: Option<PathBuf>,
    },
}

#[derive(Debug, Clone, Copy, ValueEnum)]
pub(crate) enum ScaleArg {
    /// About 250 files.
    Small,
    /// About 2 500 files.
    Medium,
    /// About 25 000 files.
    Large,
}

impl From<ScaleArg> for Scale {
    fn from(arg: ScaleArg) -> Self {
        match arg {
            ScaleArg::Small => Scale::Small,
            ScaleArg::Medium => Scale::Medium,
            ScaleArg::Large => Scale::Large,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, ValueEnum)]
pub(crate) enum RetrieverArg {
    /// Keyword grep, approximating what an agent does without Knowell.
    Grep,
    /// Knowell's code-aware BM25 index.
    Bm25,
    /// The real engine with deterministic local embeddings (64 dimensions).
    Hybrid,
}

pub(crate) fn run(
    cmd: EvalCommand,
    lexical_spans: u8,
    out: &mut Output,
) -> anyhow::Result<ExitCode> {
    match cmd {
        EvalCommand::Generate {
            out: dir,
            seed,
            scale,
            git,
        } => {
            let fixture = generate(&FixtureSpec {
                seed,
                scale: scale.into(),
            });
            let manifest = fixture
                .write_to(&dir, &WriteOptions { git })
                .with_context(|| format!("cannot write the fixture to {}", dir.display()))?;
            let manifest_path = dir.join("fixture-manifest.json");
            std::fs::write(&manifest_path, manifest.to_json()?)
                .with_context(|| format!("cannot write {}", manifest_path.display()))?;
            out.line(format!(
                "wrote {} ({} projects, {} files, tree {}) to {}",
                fixture.name(),
                fixture.projects().len(),
                fixture.file_count(),
                fixture.tree_hash().short(),
                dir.display()
            ))?;
            out.flush()?;
            Ok(ExitCode::SUCCESS)
        }
        EvalCommand::Run {
            seed,
            scale,
            retrievers,
            depth,
            json,
            markdown,
            baseline,
            tolerance,
            keep,
            bm25_coordination,
            database_url,
        } => {
            let opts = RunOptions {
                lexical_spans,
                bm25_coordination,
                spec: FixtureSpec {
                    seed,
                    scale: scale.into(),
                },
                retrievers,
                depth,
                json,
                markdown,
                baseline,
                tolerance,
                keep,
                database_url,
                live: None,
            };
            run_eval(opts, out)
        }
        EvalCommand::Live {
            database_url,
            api_key_ref,
            dimensions,
            max_tokens,
            json,
            markdown,
        } => {
            let live = crate::eval_hybrid::LiveOptions::new(&api_key_ref, dimensions, max_tokens)?;
            run_eval(
                RunOptions {
                    lexical_spans,
                    database_url: Some(database_url),
                    live: Some(live),
                    bm25_coordination: None,
                    spec: FixtureSpec {
                        seed: 42,
                        scale: Scale::Small,
                    },
                    retrievers: vec![RetrieverArg::Grep, RetrieverArg::Bm25, RetrieverArg::Hybrid],
                    depth: 10,
                    json: Some(json),
                    markdown,
                    baseline: None,
                    tolerance: 1e-4,
                    keep: None,
                },
                out,
            )
        }
    }
}

struct RunOptions {
    lexical_spans: u8,
    live: Option<crate::eval_hybrid::LiveOptions>,
    database_url: Option<String>,
    bm25_coordination: Option<f32>,
    spec: FixtureSpec,
    retrievers: Vec<RetrieverArg>,
    depth: usize,
    json: Option<PathBuf>,
    markdown: Option<PathBuf>,
    baseline: Option<PathBuf>,
    tolerance: f64,
    keep: Option<PathBuf>,
}

fn run_eval(opts: RunOptions, out: &mut Output) -> anyhow::Result<ExitCode> {
    let hybrid = opts.retrievers.contains(&RetrieverArg::Hybrid);
    if !hybrid && opts.lexical_spans != 1 {
        bail!("--lexical-spans requires --retriever hybrid for evaluation");
    }
    if hybrid && opts.database_url.is_none() {
        bail!("hybrid requires --database-url with an env:NAME or file:/path reference");
    }
    if !hybrid && opts.database_url.is_some() {
        bail!("--database-url requires --retriever hybrid");
    }
    if hybrid && opts.bm25_coordination.is_some() {
        bail!(
            "--bm25-coordination does not configure the hybrid engine; run this experiment separately"
        );
    }
    if opts.depth < knowell_eval::RANK_CUTOFF {
        bail!("evaluation depth must be at least 10");
    }
    let fixture = generate(&opts.spec);
    let queries = QuerySet::builtin()?;
    queries.validate_against(&fixture)?;

    // The workspace is written to disk and walked with the real walker so
    // the measurement sees exactly what Knowell would index.
    let temp;
    let root: &Path = match &opts.keep {
        Some(dir) => dir,
        None => {
            temp = tempfile::tempdir().context("cannot create a temporary directory")?;
            temp.path()
        }
    };
    fixture
        .write_to(root, &WriteOptions { git: hybrid })
        .with_context(|| format!("cannot write the fixture to {}", root.display()))?;
    let walked = walk_fixture(root, &fixture)?;
    tracing::info!(
        "indexed {} documents; secret boundary: {} file(s) excluded by path, {} document(s) redacted",
        walked.corpus.len(),
        walked.excluded.len(),
        walked.redactions.len()
    );

    let (report, evidence) = match &opts.database_url {
        Some(reference) => crate::eval_hybrid::measure(
            root,
            &fixture,
            reference,
            opts.live.as_ref(),
            opts.lexical_spans,
            |hybrid| measure(&fixture, &walked.corpus, &queries, &opts, Some(hybrid)),
        )?,
        None => (
            measure(&fixture, &walked.corpus, &queries, &opts, None)?,
            None,
        ),
    };
    let engine_spans = hybrid.then_some(opts.lexical_spans);
    let mut markdown = render_report_markdown(&report, evidence.as_ref(), engine_spans);
    let mut regressed = false;
    if let Some(path) = &opts.baseline {
        let text = std::fs::read_to_string(path).with_context(|| {
            format!(
                "cannot read baseline {}; create it with `know eval run --json {}`",
                path.display(),
                path.display()
            )
        })?;
        let baseline = Report::from_json(&text)?;
        let comparison = knowell_eval::compare(&report, &baseline, opts.tolerance)?;
        regressed = comparison.has_regressions();
        markdown.push('\n');
        markdown.push_str(&comparison.to_markdown());
    }

    if let Some(path) = &opts.json {
        let text = render_report_json(&report, evidence.as_ref(), engine_spans)?;
        std::fs::write(path, text).with_context(|| format!("cannot write {}", path.display()))?;
    }
    match &opts.markdown {
        Some(path) => append(path, &markdown)?,
        None => out.line(markdown.trim_end())?,
    }
    out.flush()?;

    if regressed {
        tracing::error!("retrieval quality regressed against the baseline");
        return Ok(ExitCode::FAILURE);
    }
    Ok(ExitCode::SUCCESS)
}

fn render_report_markdown(
    report: &Report,
    evidence: Option<&crate::eval_hybrid::LiveEvidence>,
    engine_spans: Option<u8>,
) -> String {
    let mut markdown = report.to_markdown();
    if let Some(spans) = engine_spans {
        let mode = if spans == 1 {
            "default"
        } else {
            "explicit experiment"
        };
        markdown.push_str(&format!(
            "\n## Engine retrieval conditions\n\nLexical spans per pinned file: {spans} ({mode}). Default engine weights, retrieval quotas and chunking; spans share the existing candidate quotas and use file-first waves.\n"
        ));
    }
    if let Some(evidence) = evidence {
        markdown.push_str(&evidence.markdown());
    }
    markdown
}

fn render_report_json(
    report: &Report,
    evidence: Option<&crate::eval_hybrid::LiveEvidence>,
    engine_spans: Option<u8>,
) -> anyhow::Result<String> {
    if evidence.is_none() && engine_spans.is_none() {
        // Standalone grep/BM25 reports retain the existing baseline format.
        return Ok(report.to_json()?);
    }
    let mut value = serde_json::to_value(report)?;
    let object = value
        .as_object_mut()
        .context("evaluation report is not an object")?;
    if let Some(spans) = engine_spans {
        object.insert(
            "engine_retrieval".into(),
            serde_json::json!({ "lexical_spans_per_file": spans }),
        );
    }
    if let Some(evidence) = evidence {
        object.insert(
            "live_embedding_conditions".into(),
            serde_json::to_value(evidence)?,
        );
    }
    Ok(serde_json::to_string_pretty(&value)?)
}

fn measure(
    fixture: &Fixture,
    corpus: &knowell_eval::Corpus,
    queries: &QuerySet,
    opts: &RunOptions,
    hybrid: Option<&dyn Retriever>,
) -> anyhow::Result<Report> {
    let mut wanted = if opts.retrievers.is_empty() {
        vec![RetrieverArg::Grep, RetrieverArg::Bm25]
    } else {
        opts.retrievers.clone()
    };
    wanted.sort();
    wanted.dedup();

    let mut owned: Vec<Box<dyn Retriever>> = Vec::new();
    for kind in wanted {
        match kind {
            RetrieverArg::Grep => owned.push(Box::new(GrepRetriever::new(corpus))),
            RetrieverArg::Bm25 => {
                let mut options = knowell_lexical::SearchOptions::default();
                if let Some(c) = opts.bm25_coordination {
                    options.coordination = c;
                }
                owned.push(Box::new(Bm25Retriever::with_options(corpus, options)?));
            }
            RetrieverArg::Hybrid => {}
        }
    }
    let mut refs: Vec<&dyn Retriever> = owned.iter().map(AsRef::as_ref).collect();
    if let Some(hybrid) = hybrid {
        refs.push(hybrid);
    }
    if refs.is_empty() {
        bail!("no retriever selected");
    }
    tracing::debug!("fixture {} tree {}", fixture.name(), fixture.tree_hash());
    Ok(knowell_eval::run(corpus, queries, &refs, opts.depth)?)
}

fn append(path: &Path, text: &str) -> anyhow::Result<()> {
    let mut file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .with_context(|| format!("cannot open {}", path.display()))?;
    file.write_all(text.as_bytes())
        .with_context(|| format!("cannot write {}", path.display()))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::indexing_slicing)]

    use super::*;

    #[test]
    fn engine_span_reports_record_default_and_experimental_conditions() {
        let report = Report::from_json(include_str!(
            "../../../eval/baselines/synthetic-small-hybrid.json"
        ))
        .unwrap();
        let original = report.to_json().unwrap();
        assert_eq!(render_report_json(&report, None, None).unwrap(), original);
        assert_eq!(
            render_report_markdown(&report, None, None),
            report.to_markdown()
        );
        for spans in [1, 2, 3] {
            let text = render_report_json(&report, None, Some(spans)).unwrap();
            let value: serde_json::Value = serde_json::from_str(&text).unwrap();
            assert_eq!(value["engine_retrieval"]["lexical_spans_per_file"], spans);
            assert!(value.get("live_embedding_conditions").is_none());
            assert_eq!(
                Report::from_json(&text).unwrap().to_json().unwrap(),
                original
            );
            assert_eq!(
                render_report_json(&report, None, Some(spans)).unwrap(),
                text
            );
            let mode = if spans == 1 {
                "default"
            } else {
                "explicit experiment"
            };
            assert!(
                render_report_markdown(&report, None, Some(spans))
                    .contains(&format!("Lexical spans per pinned file: {spans} ({mode})"))
            );
        }
    }
}
