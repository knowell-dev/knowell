//! T0 change planning: what a new generation contains and how it differs
//! from the active one.
//!
//! | Situation | Plan |
//! |---|---|
//! | no active generation | **initial**: every file of the commit |
//! | active commit is an ancestor of the target | **incremental**: `git diff` names the changed paths (renames included); only those blobs are read |
//! | not an ancestor (force-push, rebase, reset) | **rewrite**: full re-walk of the target's tree; history-based assumptions are dropped, but blobs already known from the previous manifest are not read again and stored content, chunks and embeddings are reused by content hash |
//! | forced (reconciliation) | **rebuild**: full re-walk ignoring the blob cache |
//! | directory source | **directory**: walk the directory |
//!
//! Remote-branch and tag views are read from git objects; the user's
//! checkout is never touched. Planning is blocking work (git, file reads).

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use knowell_core::{ContentHash, RepoPath};
use knowell_source::git::{Change, EntryKind, GitError, GitRepo};
use knowell_source::{SkipReason, SourceFile, WalkOptions};
use knowell_store::content::FileChange;

use crate::config::{GitConfigMode, Limits};
use crate::context::ViewContext;
use crate::error::IndexError;
use crate::jobs::BuildTarget;
use crate::manifest::{Entry, EntryState, Manifest};

/// Whether a skip decision depends only on the blob's bytes (and the size
/// limit, which is part of the policy hash), so it can be cached by blob id.
pub(crate) fn is_blob_decision(reason: &SkipReason) -> bool {
    matches!(
        reason,
        SkipReason::TooLarge { .. } | SkipReason::Binary | SkipReason::NotUtf8
    )
}

/// How a generation was planned.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum PlanKind {
    /// First generation of the view.
    Initial,
    /// The target descends from the active commit; planned from the diff.
    Incremental,
    /// History was rewritten; full re-walk with content reuse.
    Rewrite,
    /// Forced full re-walk (reconciliation found the store out of step).
    Rebuild,
    /// Directory source: full walk.
    Directory,
}

/// What [`plan`] produced.
pub(crate) struct Plan {
    pub(crate) kind: PlanKind,
    /// File changes to apply to the generation, sorted by path.
    pub(crate) changes: Vec<FileChange>,
    /// Files read for this plan (redacted text), by project path.
    pub(crate) files: BTreeMap<RepoPath, SourceFile>,
    /// Manifest of the new generation.
    pub(crate) manifest: Manifest,
    /// Files not indexed (excluded, too large, binary, ...).
    pub(crate) skipped: usize,
}

/// Inputs of [`plan`].
pub(crate) struct PlanInput<'a> {
    pub(crate) ctx: &'a ViewContext,
    pub(crate) generation: i64,
    pub(crate) target: &'a BuildTarget,
    /// Files of the active generation (path → content hash).
    pub(crate) base_files: &'a BTreeMap<RepoPath, ContentHash>,
    /// Commit of the active generation.
    pub(crate) base_commit: Option<&'a str>,
    /// Manifest of the active generation, if present and built with the
    /// same content policy.
    pub(crate) base_manifest: Option<&'a Manifest>,
    pub(crate) limits: Limits,
    pub(crate) force: bool,
    pub(crate) git_config: GitConfigMode,
}

/// Opens a repository with the configured git configuration mode.
pub(crate) fn open_repo(path: &Path, mode: GitConfigMode) -> Result<GitRepo, GitError> {
    match mode {
        GitConfigMode::User => GitRepo::open(path),
        GitConfigMode::Isolated => GitRepo::open_isolated(path),
    }
}

fn walk_options(limits: &Limits) -> WalkOptions {
    WalkOptions {
        max_file_bytes: limits.max_file_bytes,
        respect_gitignore: true,
        follow_symlinks: false,
    }
}

/// Plans generation `input.generation`.
pub(crate) fn plan(input: &PlanInput<'_>) -> Result<Plan, IndexError> {
    match input.target {
        BuildTarget::Commit { id } => plan_commit(input, id),
        BuildTarget::Tree { .. } => plan_directory(input),
    }
}

/// The changes that turn `base` into `new`. `renames` maps new paths to the
/// old paths git saw them move from; a rename is recorded only when the old
/// path is gone and the new one is new or changed.
pub(crate) fn compute_changes(
    base: &BTreeMap<RepoPath, ContentHash>,
    new: &BTreeMap<RepoPath, ContentHash>,
    renames: &BTreeMap<RepoPath, RepoPath>,
) -> Vec<FileChange> {
    let mut changes = Vec::new();
    let mut moved_from = BTreeSet::new();
    for (path, hash) in new {
        if base.get(path) == Some(hash) {
            continue;
        }
        let renamed_from = renames
            .get(path)
            .filter(|from| base.contains_key(*from) && !new.contains_key(*from))
            .filter(|from| !moved_from.contains(*from))
            .cloned();
        if let Some(from) = &renamed_from {
            moved_from.insert(from.clone());
        }
        changes.push(FileChange::Upsert {
            path: path.clone(),
            content_hash: *hash,
            renamed_from,
        });
    }
    for path in base.keys() {
        if !new.contains_key(path) && !moved_from.contains(path) {
            changes.push(FileChange::Delete { path: path.clone() });
        }
    }
    changes
}

fn check_count(ctx: &ViewContext, count: usize, limits: &Limits) -> Result<(), IndexError> {
    if count > limits.max_files_per_view {
        return Err(IndexError::Limit(format!(
            "project `{}` has {count} indexable files, above the limit of {}; raise `limits.max_files_per_view` or exclude paths",
            ctx.project_name, limits.max_files_per_view
        )));
    }
    Ok(())
}

struct Candidate {
    path: RepoPath,
    repo_path: RepoPath,
    blob: String,
}

fn plan_commit(input: &PlanInput<'_>, commit: &str) -> Result<Plan, IndexError> {
    let ctx = input.ctx;
    let repo = open_repo(&ctx.source_path, input.git_config)?;
    let mut skipped = 0usize;
    let mut candidates = Vec::new();
    for entry in repo.list_tree(commit)? {
        if entry.mode == EntryKind::Submodule {
            continue;
        }
        let Some(path) = ctx.to_project_path(&entry.path) else {
            continue;
        };
        // Excluded paths are never read; the decision is the path's alone.
        if ctx.policy.check(&path).is_some() || entry.mode == EntryKind::Symlink {
            skipped += 1;
            continue;
        }
        candidates.push(Candidate {
            path,
            repo_path: entry.path,
            blob: entry.blob,
        });
    }

    let kind = match input.base_commit {
        _ if input.force => PlanKind::Rebuild,
        None if input.base_files.is_empty() => PlanKind::Initial,
        None => PlanKind::Rewrite,
        Some(base) => match repo.is_ancestor(base, commit) {
            Ok(true) => PlanKind::Incremental,
            Ok(false) => PlanKind::Rewrite,
            // The old commit is gone (rewritten and pruned): nothing to
            // diff against, so re-walk.
            Err(GitError::CommitNotFound { .. }) => PlanKind::Rewrite,
            Err(e) => return Err(e.into()),
        },
    };

    // Paths git reports as changed since the active commit (incremental
    // plans only), and renames among them.
    let mut changed: Option<BTreeSet<RepoPath>> = None;
    let mut renames = BTreeMap::new();
    if kind == PlanKind::Incremental
        && let Some(base) = input.base_commit
    {
        let mut set = BTreeSet::new();
        for change in repo.diff(base, commit)? {
            let new = ctx.to_project_path(change.path());
            let old = change.old_path().and_then(|p| ctx.to_project_path(p));
            if let (Change::Renamed { .. }, Some(to), Some(from)) = (&change, &new, &old) {
                renames.insert(to.clone(), from.clone());
            }
            set.extend(new);
            set.extend(old);
        }
        changed = Some(set);
    }

    let cache = if input.force {
        BTreeMap::new()
    } else {
        input
            .base_manifest
            .map(Manifest::blob_cache)
            .unwrap_or_default()
    };
    let mut states: BTreeMap<RepoPath, (String, EntryState)> = BTreeMap::new();
    let mut to_read = Vec::new();
    for candidate in candidates {
        let known = cache
            .get(candidate.blob.as_str())
            .map(|s| (*s).clone())
            .or_else(|| {
                let unchanged = changed
                    .as_ref()
                    .is_some_and(|set| !set.contains(&candidate.path));
                unchanged
                    .then(|| input.base_files.get(&candidate.path))
                    .flatten()
                    .map(|hash| EntryState::Indexed { hash: *hash })
            });
        match known {
            Some(state) => {
                states.insert(candidate.path, (candidate.blob, state));
            }
            None => to_read.push(candidate),
        }
    }

    let mut files = BTreeMap::new();
    if !to_read.is_empty() {
        let repo_paths: Vec<RepoPath> = to_read.iter().map(|c| c.repo_path.clone()).collect();
        let by_repo_path: BTreeMap<&RepoPath, &Candidate> =
            to_read.iter().map(|c| (&c.repo_path, c)).collect();
        let report = repo.read_commit_files(
            commit,
            &repo_paths,
            &ctx.policy,
            &walk_options(&input.limits),
        )?;
        for file in report.files {
            let Some(candidate) = by_repo_path.get(&file.path) else {
                continue;
            };
            states.insert(
                candidate.path.clone(),
                (
                    candidate.blob.clone(),
                    EntryState::Indexed { hash: file.hash },
                ),
            );
            files.insert(
                candidate.path.clone(),
                SourceFile {
                    path: candidate.path.clone(),
                    ..file
                },
            );
        }
        for skip in report.skipped {
            skipped += 1;
            let Some(candidate) = by_repo_path.get(&skip.path) else {
                continue;
            };
            if is_blob_decision(&skip.reason) {
                states.insert(
                    candidate.path.clone(),
                    (
                        candidate.blob.clone(),
                        EntryState::Skipped {
                            reason: skip.reason.as_str().to_owned(),
                        },
                    ),
                );
            }
        }
    }

    let mut new_files = BTreeMap::new();
    let mut entries = Vec::with_capacity(states.len());
    for (path, (blob, state)) in states {
        match &state {
            EntryState::Indexed { hash } => {
                new_files.insert(path.clone(), *hash);
            }
            EntryState::Skipped { .. } => skipped += 1,
        }
        entries.push(Entry {
            path,
            blob: Some(blob),
            state,
        });
    }
    check_count(ctx, new_files.len(), &input.limits)?;
    let changes = compute_changes(input.base_files, &new_files, &renames);
    Ok(Plan {
        kind,
        changes,
        files,
        manifest: Manifest::new(
            ctx.view,
            input.generation,
            Some(commit.to_owned()),
            ctx.policy_key,
            entries,
        ),
        skipped,
    })
}

fn plan_directory(input: &PlanInput<'_>) -> Result<Plan, IndexError> {
    let ctx = input.ctx;
    let report = knowell_source::walk(
        &ctx.project_dir(),
        &ctx.policy,
        &walk_options(&input.limits),
    )?;
    let mut skipped = 0usize;
    let mut entries = Vec::new();
    let mut new_files = BTreeMap::new();
    let mut files = BTreeMap::new();
    for file in report.files {
        new_files.insert(file.path.clone(), file.hash);
        entries.push(Entry {
            path: file.path.clone(),
            blob: None,
            state: EntryState::Indexed { hash: file.hash },
        });
        files.insert(file.path.clone(), file);
    }
    for skip in report.skipped {
        skipped += 1;
        if is_blob_decision(&skip.reason) {
            entries.push(Entry {
                path: skip.path,
                blob: None,
                state: EntryState::Skipped {
                    reason: skip.reason.as_str().to_owned(),
                },
            });
        }
    }
    check_count(ctx, new_files.len(), &input.limits)?;
    let changes = compute_changes(input.base_files, &new_files, &BTreeMap::new());
    Ok(Plan {
        kind: PlanKind::Directory,
        changes,
        files,
        manifest: Manifest::new(ctx.view, input.generation, None, ctx.policy_key, entries),
        skipped,
    })
}

/// Tree hash of a directory source as it is on disk now (reads every file).
pub(crate) fn directory_tree_hash(
    ctx: &ViewContext,
    limits: &Limits,
) -> Result<ContentHash, IndexError> {
    let report = knowell_source::walk(&ctx.project_dir(), &ctx.policy, &walk_options(limits))?;
    Ok(crate::merkle::tree_hash(
        report.files.iter().map(|f| (&f.path, &f.hash)),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(s: &str) -> RepoPath {
        RepoPath::new(s).unwrap()
    }

    fn h(s: &str) -> ContentHash {
        ContentHash::of(s.as_bytes())
    }

    fn map(items: &[(&str, &str)]) -> BTreeMap<RepoPath, ContentHash> {
        items
            .iter()
            .map(|(path, content)| (p(path), h(content)))
            .collect()
    }

    #[test]
    fn changes_cover_upserts_deletes_and_renames() {
        let base = map(&[("a", "1"), ("b", "2"), ("c", "3"), ("old", "4")]);
        let new = map(&[("a", "1"), ("b", "2x"), ("d", "5"), ("new", "4")]);
        let renames: BTreeMap<RepoPath, RepoPath> = [(p("new"), p("old"))].into_iter().collect();
        let changes = compute_changes(&base, &new, &renames);
        assert_eq!(
            changes,
            vec![
                FileChange::Upsert {
                    path: p("b"),
                    content_hash: h("2x"),
                    renamed_from: None
                },
                FileChange::Upsert {
                    path: p("d"),
                    content_hash: h("5"),
                    renamed_from: None
                },
                FileChange::Upsert {
                    path: p("new"),
                    content_hash: h("4"),
                    renamed_from: Some(p("old"))
                },
                FileChange::Delete { path: p("c") },
            ]
        );
    }

    #[test]
    fn blob_decisions_are_the_content_ones() {
        assert!(is_blob_decision(&SkipReason::NotUtf8));
        assert!(is_blob_decision(&SkipReason::TooLarge { size: 1 }));
        assert!(!is_blob_decision(&SkipReason::Symlink));
        assert!(!is_blob_decision(&SkipReason::Unreadable {
            error_kind: "PermissionDenied".into()
        }));
    }

    #[test]
    fn renames_need_a_vanished_source() {
        // `old` still exists: a copy, not a rename.
        let base = map(&[("old", "4")]);
        let new = map(&[("old", "4"), ("new", "4")]);
        let renames: BTreeMap<RepoPath, RepoPath> = [(p("new"), p("old"))].into_iter().collect();
        let changes = compute_changes(&base, &new, &renames);
        assert_eq!(
            changes,
            vec![FileChange::Upsert {
                path: p("new"),
                content_hash: h("4"),
                renamed_from: None
            }]
        );
        // Identical sides produce nothing.
        assert!(compute_changes(&base, &base, &BTreeMap::new()).is_empty());
    }
}
