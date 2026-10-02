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
        /// Retrievers to measure (repeatable). Default: all available.
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
}

pub(crate) fn run(cmd: EvalCommand, out: &mut Output) -> anyhow::Result<ExitCode> {
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
        } => {
            let opts = RunOptions {
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
            };
            run_eval(opts, out)
        }
    }
}

struct RunOptions {
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
        .write_to(root, &WriteOptions { git: false })
        .with_context(|| format!("cannot write the fixture to {}", root.display()))?;
    let walked = walk_fixture(root, &fixture)?;
    tracing::info!(
        "indexed {} documents; secret boundary: {} file(s) excluded by path, {} document(s) redacted",
        walked.corpus.len(),
        walked.excluded.len(),
        walked.redactions.len()
    );

    let report = measure(&fixture, &walked.corpus, &queries, &opts)?;
    let mut markdown = report.to_markdown();
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
        std::fs::write(path, report.to_json()?)
            .with_context(|| format!("cannot write {}", path.display()))?;
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

fn measure(
    fixture: &Fixture,
    corpus: &knowell_eval::Corpus,
    queries: &QuerySet,
    opts: &RunOptions,
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
        }
    }
    let refs: Vec<&dyn Retriever> = owned.iter().map(AsRef::as_ref).collect();
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
