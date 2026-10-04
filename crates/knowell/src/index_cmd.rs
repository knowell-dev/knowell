//! `know index`: run one local workspace's durable indexing jobs to idle.
//!
//! Idle is not a completion guarantee: retry delays, superseded builds and
//! failed embedding tiers remain explicit and make the command exit with 1.

use std::io::Read;
use std::path::{Component, Path, PathBuf};
use std::process::ExitCode;

use clap::Args;
use knowell_core::Name;
use knowell_index::{
    BuildTarget, PreparedScipImport, Priority, RegistrationIssue, RunSummary, ScipImportLimits,
    ScipImportManifest, SyncOutcome,
};
use serde::Serialize;

use crate::db;
use crate::env::Env;
use crate::local_engine::{self, LocalArgs, LocalEngine, Prepared, ProjectStatus};
use crate::output::Output;

#[derive(Debug, Args)]
pub(crate) struct IndexArgs {
    #[command(flatten)]
    local: LocalArgs,
    /// Rebuild unchanged sources with the current policy and embedding profile;
    /// configured cloud providers may incur charges.
    #[arg(long)]
    rebuild: bool,
    /// Import an already produced SCIP artifact in a new analysis generation.
    /// Does not run an external indexer; unchanged vectors are reusable.
    #[arg(long, requires_all = ["scip_manifest", "project"])]
    scip_index: Option<PathBuf>,
    /// Version/hash/compiler-input manifest captured with --scip-index.
    #[arg(long, requires_all = ["scip_index", "project"])]
    scip_manifest: Option<PathBuf>,
    /// Project owning the explicit SCIP artifact; requires exactly one view.
    #[arg(long, requires = "scip_index")]
    project: Option<Name>,
    /// Print indexing outcomes and freshness as JSON.
    #[arg(long)]
    json: bool,
}

#[derive(Debug, Serialize)]
struct IndexedProject {
    #[serde(flatten)]
    status: ProjectStatus,
    sync: SyncOutcome,
    complete: bool,
    incomplete_reasons: Vec<String>,
}

#[derive(Debug, Serialize)]
struct IndexReport {
    workspace: Name,
    complete: bool,
    run: RunSummary,
    issues: Vec<RegistrationIssue>,
    projects: Vec<IndexedProject>,
    incomplete_reasons: Vec<String>,
}

/// Runs indexing once; returns 1 for an incomplete target and errors for failed operations.
pub(crate) fn run(args: IndexArgs, env: &Env, out: &mut Output) -> anyhow::Result<ExitCode> {
    let scip = load_scip_import(&args)?;
    let prepared = Prepared::load(env, args.local.organization)?;
    let report = db::runtime()?.block_on(async {
        let local = prepared.open(env).await?;
        let report = index(&local, args.rebuild, scip.as_ref()).await;
        local.store().close().await;
        report
    })?;
    if args.json {
        out.line(serde_json::to_string_pretty(&report)?)?;
    } else {
        out.line(format!(
            "workspace `{}`: indexing {}",
            report.workspace,
            if report.complete {
                "complete"
            } else {
                "incomplete"
            }
        ))?;
        out.line(format!(
            "  {} job(s) run; {} succeeded; {} failed",
            report.run.jobs, report.run.succeeded, report.run.failed
        ))?;
        for issue in &report.issues {
            out.line(format!(
                "  project `{}` could not be registered: {}",
                issue.project,
                local_engine::terminal_text(&issue.reason)
            ))?;
        }
        for project in &report.projects {
            crate::status_cmd::write_project(&project.status, out)?;
            for reason in &project.incomplete_reasons {
                out.line(format!("    {}", local_engine::terminal_text(reason)))?;
            }
        }
        for reason in &report.incomplete_reasons {
            out.line(format!("  {}", local_engine::terminal_text(reason)))?;
        }
        if !report.complete {
            out.line(
                "inspect `know status`; run `know index` again when the reported cause is resolved",
            )?;
        }
    }
    out.flush()?;
    Ok(if report.complete {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    })
}

async fn index(
    local: &LocalEngine,
    rebuild: bool,
    scip: Option<&(Name, PreparedScipImport)>,
) -> anyhow::Result<IndexReport> {
    if let Some((name, _)) = scip {
        let matches = local
            .registration
            .views
            .iter()
            .filter(|view| &view.project == name)
            .count();
        anyhow::ensure!(
            matches == 1,
            "scip import requires exactly one registered view for the selected project"
        );
    }
    let mut requested = Vec::with_capacity(local.registration.views.len());
    for registered in &local.registration.views {
        let previous = local.status(registered).await?;
        let sync = if let Some((_, artifact)) = scip.filter(|(name, _)| name == &registered.project)
        {
            local
                .engine
                .indexer()
                .rebuild_view_with_scip(registered.view, artifact, Priority::Interactive)
                .await?
        } else if rebuild {
            local
                .engine
                .indexer()
                .rebuild_view(registered.view, Priority::Interactive)
                .await?
        } else {
            local
                .engine
                .indexer()
                .refresh_view(registered.view, Priority::Interactive)
                .await?
        };
        requested.push((registered, previous, sync));
    }
    let run = local
        .engine
        .indexer()
        .run_until_idle_scoped_with(local.engine.indexer().config().concurrency)
        .await?;
    let mut projects = Vec::with_capacity(requested.len());
    for (registered, previous, sync) in requested {
        let status = local.status(registered).await?;
        let reasons = completion_reasons(&status, &previous, &sync);
        projects.push(IndexedProject {
            status,
            sync,
            complete: reasons.is_empty(),
            incomplete_reasons: reasons,
        });
    }
    let mut reasons = Vec::new();
    if projects.is_empty() {
        reasons.push("the workspace has no registered project views".to_owned());
    }
    if run.failed > 0 {
        reasons.push("some indexing jobs failed; retry-delayed jobs may remain queued".to_owned());
    }
    let issues = local.registration.issues.clone();
    let complete =
        issues.is_empty() && reasons.is_empty() && projects.iter().all(|project| project.complete);
    Ok(IndexReport {
        workspace: local.workspace.name.clone(),
        complete,
        run,
        issues,
        projects,
        incomplete_reasons: reasons,
    })
}

fn load_scip_import(args: &IndexArgs) -> anyhow::Result<Option<(Name, PreparedScipImport)>> {
    let (Some(index), Some(manifest), Some(project)) =
        (&args.scip_index, &args.scip_manifest, &args.project)
    else {
        anyhow::ensure!(
            args.scip_index.is_none() && args.scip_manifest.is_none() && args.project.is_none(),
            "scip import requires --scip-index, --scip-manifest and --project together"
        );
        return Ok(None);
    };
    let limits = ScipImportLimits::default();
    let manifest_bytes = read_analysis_file(manifest, limits.max_bytes)?;
    let manifest: ScipImportManifest = serde_json::from_slice(&manifest_bytes).map_err(|_| {
        anyhow::anyhow!(
            "invalid scip manifest; provide its version, artifact and build-input hashes"
        )
    })?;
    let index_bytes = read_analysis_file(index, limits.max_bytes)?;
    let prepared = PreparedScipImport::from_bytes(&index_bytes, manifest, &limits)?;
    Ok(Some((project.clone(), prepared)))
}

fn read_analysis_file(path: &Path, max_bytes: usize) -> anyhow::Result<Vec<u8>> {
    // Check the caller's spelling before resolving links, then check the target.
    // An excluded alias must not become readable merely because it points to an
    // allowed file, and an allowed alias must not expose a sensitive target.
    ensure_analysis_path_allowed(path)?;
    let checked = path
        .canonicalize()
        .map_err(|_| anyhow::anyhow!("cannot resolve the explicit analysis file"))?;
    ensure_analysis_path_allowed(&checked)?;
    let max = u64::try_from(max_bytes)
        .map_err(|_| anyhow::anyhow!("analysis file budget is unsupported"))?;
    // Inspect before opening: opening a FIFO can block without any content read.
    let metadata = std::fs::metadata(&checked)
        .map_err(|_| anyhow::anyhow!("cannot inspect the explicit analysis file"))?;
    anyhow::ensure!(
        metadata.is_file() && metadata.len() <= max,
        "analysis file exceeds its byte budget or is not a regular file"
    );
    let file = std::fs::File::open(&checked)
        .map_err(|_| anyhow::anyhow!("cannot open the explicit analysis file"))?;
    let opened = file
        .metadata()
        .map_err(|_| anyhow::anyhow!("cannot inspect the explicit analysis file"))?;
    anyhow::ensure!(
        opened.is_file() && opened.len() <= max,
        "analysis file exceeds its byte budget or is not a regular file"
    );
    #[cfg(unix)]
    ensure_analysis_file_identity(&metadata, &opened)?;
    let mut bytes = Vec::new();
    file.take(max.saturating_add(1))
        .read_to_end(&mut bytes)
        .map_err(|_| anyhow::anyhow!("cannot read the explicit analysis file"))?;
    anyhow::ensure!(
        bytes.len() <= max_bytes,
        "analysis file exceeds its byte budget"
    );
    Ok(bytes)
}

#[cfg(unix)]
fn ensure_analysis_file_identity(
    approved: &std::fs::Metadata,
    opened: &std::fs::Metadata,
) -> anyhow::Result<()> {
    use std::os::unix::fs::MetadataExt;

    anyhow::ensure!(
        approved.dev() == opened.dev() && approved.ino() == opened.ino(),
        "analysis file changed while being opened"
    );
    Ok(())
}

fn ensure_analysis_path_allowed(path: &Path) -> anyhow::Result<()> {
    let components = path
        .components()
        .filter_map(|component| match component {
            Component::Normal(value) => Some(value),
            _ => None,
        })
        .map(|value| {
            value
                .to_str()
                .ok_or_else(|| anyhow::anyhow!("the analysis file path is unsupported"))
        })
        .collect::<anyhow::Result<Vec<_>>>()?;
    let relative = knowell_core::RepoPath::new(components.join("/"))
        .map_err(|_| anyhow::anyhow!("the analysis file path is unsupported"))?;
    anyhow::ensure!(
        knowell_secrets::ExclusionPolicy::builtin()
            .check(&relative)
            .is_none(),
        "the analysis file is excluded by the source data policy"
    );
    Ok(())
}

fn completion_reasons(
    project: &ProjectStatus,
    previous: &ProjectStatus,
    sync: &SyncOutcome,
) -> Vec<String> {
    let mut reasons = local_engine::incomplete_reasons(project);
    if reasons
        .iter()
        .any(|reason| reason == "embedding coverage of the active generation is incomplete")
    {
        reasons.push(
            "if the embedding provider, model or dimensions changed, run `know index --rebuild` to build the selected profile; configured cloud providers may incur charges".to_owned(),
        );
    }
    match sync {
        SyncOutcome::Failed { reason, .. } => {
            reasons.push(format!("target resolution failed: {reason}"))
        }
        SyncOutcome::UpToDate {
            commit: Some(commit),
            ..
        } => {
            if project.status.active_commit.as_ref() != Some(commit) {
                reasons.push("the active index no longer matches the requested commit".to_owned());
            }
        }
        SyncOutcome::UpToDate { commit: None, .. } => {
            if project.active_tree_hash.is_none()
                || project.active_tree_hash != previous.active_tree_hash
            {
                reasons.push(
                    "the active index no longer matches the observed directory tree".to_owned(),
                );
            }
        }
        SyncOutcome::Queued { target, .. } => {
            if project.status.active_generation <= previous.status.active_generation {
                reasons.push("the requested build has not activated a newer generation".to_owned());
            }
            let matches = match target {
                BuildTarget::Commit { id } => project.status.active_commit.as_ref() == Some(id),
                BuildTarget::Tree { hash } => project.active_tree_hash.as_ref() == Some(hash),
            };
            if !matches {
                reasons
                    .push("the active index does not match the requested build target".to_owned());
            }
        }
    }
    reasons
}

#[cfg(test)]
mod tests {
    use clap::Parser;
    use knowell_core::ContentHash;
    use knowell_index::{
        EmbeddingCoverage, EmbeddingPlan, TierSkip, TierState, TierStates, ViewStatus,
    };
    use knowell_store::{JobId, ProfileId, ViewId};

    use super::*;

    #[derive(Parser)]
    struct TestIndexCommand {
        #[command(flatten)]
        index: IndexArgs,
    }

    fn import_args(index: &Path, manifest: &Path) -> IndexArgs {
        TestIndexCommand::try_parse_from([
            "know",
            "--scip-index",
            index.to_str().unwrap(),
            "--scip-manifest",
            manifest.to_str().unwrap(),
            "--project",
            "synthetic-project",
        ])
        .unwrap()
        .index
    }

    #[test]
    fn scip_import_flags_require_all_three_or_none() {
        for supplied in 0..8 {
            let mut args = vec!["know"];
            for (bit, flag, value) in [
                (1, "--scip-index", "synthetic.scip"),
                (2, "--scip-manifest", "synthetic.json"),
                (4, "--project", "synthetic-project"),
            ] {
                if supplied & bit != 0 {
                    args.extend([flag, value]);
                }
            }
            match TestIndexCommand::try_parse_from(args) {
                Ok(_) => assert!(supplied == 0 || supplied == 7),
                Err(error) => {
                    assert!(supplied != 0 && supplied != 7);
                    assert_eq!(
                        error.kind(),
                        clap::error::ErrorKind::MissingRequiredArgument
                    );
                }
            }
        }
    }

    #[test]
    fn analysis_exclusions_precede_resolution_and_do_not_echo_paths() {
        let directory = tempfile::tempdir().unwrap();
        let excluded = directory.path().join(".env.KNOWELL_CANARY_ANALYSIS_PATH");
        assert!(!excluded.exists());
        let error = read_analysis_file(&excluded, 8).unwrap_err();
        assert_eq!(
            error.to_string(),
            "the analysis file is excluded by the source data policy"
        );
        assert!(!format!("{error:#}").contains("KNOWELL_CANARY"));
    }

    #[cfg(unix)]
    #[test]
    fn analysis_exclusions_cover_both_link_names_and_canonical_targets() {
        use std::os::unix::fs::symlink;

        let directory = tempfile::tempdir().unwrap();
        let allowed = directory.path().join("synthetic.scip");
        std::fs::write(&allowed, b"synthetic").unwrap();
        let excluded_alias = directory.path().join(".env.synthetic-link");
        symlink(&allowed, &excluded_alias).unwrap();
        let excluded_target = directory.path().join(".env.synthetic-target");
        std::fs::write(&excluded_target, b"KNOWELL_CANARY_SYNTHETIC_ENV").unwrap();
        let allowed_alias = directory.path().join("synthetic-link.scip");
        symlink(&excluded_target, &allowed_alias).unwrap();
        for path in [&excluded_alias, &allowed_alias] {
            assert_eq!(
                read_analysis_file(path, 64).unwrap_err().to_string(),
                "the analysis file is excluded by the source data policy"
            );
        }
    }

    #[test]
    fn analysis_file_byte_budget_accepts_the_limit_and_rejects_one_more_byte() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("synthetic.scip");
        std::fs::write(&path, b"12345678").unwrap();
        assert_eq!(read_analysis_file(&path, 8).unwrap(), b"12345678");
        std::fs::write(&path, b"123456789").unwrap();
        assert_eq!(
            read_analysis_file(&path, 8).unwrap_err().to_string(),
            "analysis file exceeds its byte budget or is not a regular file"
        );
        std::fs::write(&path, b"").unwrap();
        assert!(read_analysis_file(&path, 0).unwrap().is_empty());
    }

    #[test]
    fn analysis_file_refuses_a_directory_before_opening_it() {
        let directory = tempfile::tempdir().unwrap();
        assert_eq!(
            read_analysis_file(directory.path(), 8)
                .unwrap_err()
                .to_string(),
            "analysis file exceeds its byte budget or is not a regular file"
        );
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[test]
    fn analysis_file_refuses_non_regular_socket_before_opening_it() {
        // Unix sockets have a short platform path limit independent of the file
        // reader. Avoid an ambient TMPDIR that can exhaust that limit first.
        let directory = tempfile::Builder::new()
            .prefix("knowell-analysis-")
            .tempdir_in("/tmp")
            .unwrap();
        let path = directory.path().join("s");
        let _socket = std::os::unix::net::UnixListener::bind(&path).unwrap();
        assert_eq!(
            read_analysis_file(&path, 8).unwrap_err().to_string(),
            "analysis file exceeds its byte budget or is not a regular file"
        );
    }

    #[cfg(unix)]
    #[test]
    fn analysis_file_identity_rejects_a_replaced_handle_before_content_read() {
        use std::os::unix::fs::symlink;

        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("synthetic.scip");
        std::fs::write(&path, b"synthetic").unwrap();
        let approved = std::fs::metadata(&path).unwrap();
        let excluded = directory.path().join(".env.synthetic-target");
        std::fs::write(&excluded, b"KNOWELL_CANARY_SYNTHETIC_ENV").unwrap();
        std::fs::remove_file(&path).unwrap();
        symlink(&excluded, &path).unwrap();
        let handle = std::fs::File::open(&path).unwrap();
        assert_eq!(
            ensure_analysis_file_identity(&approved, &handle.metadata().unwrap())
                .unwrap_err()
                .to_string(),
            "analysis file changed while being opened"
        );
    }

    #[cfg(unix)]
    #[test]
    fn analysis_file_refuses_non_utf8_components_without_echoing_them() {
        use std::os::unix::ffi::OsStringExt;

        let directory = tempfile::tempdir().unwrap();
        let mut name = b"KNOWELL_CANARY_NON_UTF8".to_vec();
        name.push(0xff);
        let path = directory.path().join(std::ffi::OsString::from_vec(name));
        let error = read_analysis_file(&path, 8).unwrap_err();
        assert_eq!(error.to_string(), "the analysis file path is unsupported");
        assert!(!format!("{error:#}").contains("KNOWELL_CANARY"));
    }

    #[test]
    fn malformed_analysis_inputs_do_not_echo_content() {
        let directory = tempfile::tempdir().unwrap();
        let index = directory.path().join("synthetic.scip");
        let manifest = directory.path().join("synthetic.json");
        std::fs::write(&manifest, b"{KNOWELL_CANARY_INVALID_MANIFEST").unwrap();
        let args = import_args(&index, &manifest);
        let error = load_scip_import(&args).unwrap_err();
        assert!(error.to_string().starts_with("invalid scip manifest;"));
        assert!(!format!("{error:#}").contains("KNOWELL_CANARY"));

        let artifact = b"\xffKNOWELL_CANARY_INVALID_ARTIFACT";
        std::fs::write(&index, artifact).unwrap();
        let source = knowell_core::RepoPath::new("src/synthetic.rs").unwrap();
        let inputs =
            std::collections::BTreeMap::from([(source, ContentHash::of(b"synthetic source"))]);
        let valid_manifest = ScipImportManifest {
            source_revision: "a".repeat(40),
            artifact_hash: ContentHash::of(artifact),
            tool_name: "synthetic-indexer".to_owned(),
            tool_version: "1.0".to_owned(),
            compiler_identity: ContentHash::of(b"synthetic compiler"),
            build_inputs: inputs.clone(),
            documents: inputs,
        };
        std::fs::write(&manifest, serde_json::to_vec(&valid_manifest).unwrap()).unwrap();
        let error = load_scip_import(&args).unwrap_err();
        // This payload's invalid wire tag is rejected before document decoding.
        assert!(
            error
                .to_string()
                .contains("artifact wire structure is malformed or unsupported")
        );
        assert!(!format!("{error:#}").contains("KNOWELL_CANARY"));
    }

    fn directory(generation: Option<i64>, hash: Option<ContentHash>) -> ProjectStatus {
        ProjectStatus {
            status: ViewStatus {
                view: ViewId(uuid::Uuid::nil()),
                workspace: Name::new("synthetic").unwrap(),
                project: Name::new("one").unwrap(),
                target: "worktree".parse().unwrap(),
                latest_seen_commit: None,
                active_commit: None,
                active_generation: generation,
                building_generation: None,
                tiers: TierStates {
                    t0: TierState::Done,
                    t1: TierState::Done,
                    t2: TierState::Skipped {
                        reason: TierSkip::NoProvider,
                    },
                    t3: TierState::Done,
                },
                lag: None,
                last_error: None,
            },
            embedding: EmbeddingPlan::Skip {
                reason: TierSkip::NoProvider,
            },
            embedding_coverage: None,
            active_tree_hash: hash,
        }
    }

    fn queued(target: BuildTarget) -> SyncOutcome {
        SyncOutcome::Queued {
            view: ViewId(uuid::Uuid::nil()),
            target,
            job: JobId(uuid::Uuid::nil()),
            created: true,
        }
    }

    #[test]
    fn an_old_active_directory_generation_is_not_completion() {
        let old_hash = ContentHash::of(b"old synthetic tree");
        let new_hash = ContentHash::of(b"new synthetic tree");
        let previous = directory(Some(1), Some(old_hash));
        let sync = queued(BuildTarget::Tree { hash: new_hash });
        assert!(!completion_reasons(&previous, &previous, &sync).is_empty());
        let finished = directory(Some(2), Some(new_hash));
        assert!(completion_reasons(&finished, &previous, &sync).is_empty());
        let superseded = directory(Some(3), Some(ContentHash::of(b"different tree")));
        assert!(!completion_reasons(&superseded, &previous, &sync).is_empty());
    }

    #[test]
    fn failed_tiers_and_budget_skips_are_not_success() {
        let hash = ContentHash::of(b"synthetic tree");
        let previous = directory(Some(1), Some(hash));
        let sync = SyncOutcome::UpToDate {
            view: previous.status.view,
            commit: None,
        };
        for state in [
            TierState::Pending,
            TierState::Running,
            TierState::Failed {
                reason: "synthetic outage".to_owned(),
            },
            TierState::Skipped {
                reason: TierSkip::BudgetExhausted,
            },
        ] {
            let mut finished = directory(Some(1), Some(hash));
            finished.status.tiers.t2 = state;
            assert!(!completion_reasons(&finished, &previous, &sync).is_empty());
        }
        assert!(completion_reasons(&previous, &previous, &sync).is_empty());
    }

    #[test]
    fn a_new_generation_of_the_wrong_commit_is_not_completion() {
        let mut previous = directory(Some(1), None);
        previous.status.active_commit = Some("a".repeat(40));
        previous.status.latest_seen_commit = previous.status.active_commit.clone();
        let sync = queued(BuildTarget::Commit { id: "b".repeat(40) });
        let mut finished = directory(Some(2), None);
        finished.status.active_commit = Some("c".repeat(40));
        finished.status.latest_seen_commit = finished.status.active_commit.clone();
        assert!(!completion_reasons(&finished, &previous, &sync).is_empty());
        finished.status.active_commit = Some("b".repeat(40));
        finished.status.latest_seen_commit = finished.status.active_commit.clone();
        assert!(completion_reasons(&finished, &previous, &sync).is_empty());
    }

    #[test]
    fn completed_t2_requires_all_inputs_in_the_selected_generation_and_profile() {
        let hash = ContentHash::of(b"synthetic tree");
        let profile = ProfileId(uuid::Uuid::from_u128(1));
        let mut finished = directory(Some(3), Some(hash));
        finished.status.tiers.t2 = TierState::Done;
        finished.embedding = EmbeddingPlan::Embed {
            provider: Name::new("synthetic-provider").unwrap(),
            profile,
            profile_name: Name::new("synthetic-profile").unwrap(),
            dimensions: 64,
        };
        let full = EmbeddingCoverage {
            view: finished.status.view,
            generation: 3,
            profile,
            inputs: 2,
            embedded: 2,
            complete: true,
        };
        for coverage in [
            None,
            Some(EmbeddingCoverage {
                complete: false,
                ..full
            }),
            Some(EmbeddingCoverage {
                embedded: 1,
                ..full
            }),
            Some(EmbeddingCoverage {
                generation: 2,
                ..full
            }),
            Some(EmbeddingCoverage {
                profile: ProfileId(uuid::Uuid::from_u128(2)),
                ..full
            }),
        ] {
            finished.embedding_coverage = coverage;
            assert!(!local_engine::incomplete_reasons(&finished).is_empty());
        }
        finished.embedding_coverage = Some(full);
        assert!(local_engine::incomplete_reasons(&finished).is_empty());
    }
}
