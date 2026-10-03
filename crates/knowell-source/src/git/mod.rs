//! Git object access: resolving track targets, reading a commit's files
//! without touching the user's checkout, tree diffs, ancestry checks,
//! worktree discovery and a worktree's uncommitted changes.
//!
//! Everything here reads the object database and refs directly (via `gix`);
//! nothing writes to the repository, the index or the working tree.
//!
//! # No fallbacks
//!
//! [`GitRepo::resolve`] resolves exactly the requested ref. A missing branch
//! is [`GitError::RefNotFound`]; it is never replaced by `main`, `master`,
//! `HEAD` or a ref with a similar name.
//!
//! # Rewritten history
//!
//! Incremental indexing reuses the previous index of a tracked target only
//! when the previously indexed commit is an ancestor of the new one
//! ([`GitRepo::is_ancestor`]). After a force-push, rebase or reset the old
//! commit is no longer an ancestor: the indexer must then not assume that
//! "old view + diff" equals the new view's history (for example, commit
//! provenance and "introduced in" metadata are invalid). [`GitRepo::diff`]
//! still gives the correct *content* change set between the two trees, so
//! unchanged blobs (and their embeddings) can be reused by content hash.
//!
//! # Paths
//!
//! Paths are [`RepoPath`]s exactly as stored in git. Entries whose name is
//! not valid UTF-8 or not representable as a [`RepoPath`] (for example a
//! backslash, or a drive-like `c:` prefix) are reported by
//! [`GitRepo::walk_commit`] as [`SkipReason::InvalidPath`] with a lossy
//! rendition, and are omitted from [`GitRepo::list_tree`], [`GitRepo::diff`]
//! and [`GitRepo::working_changes`].
//!
//! Submodules (gitlinks) have no content in this repository: they are listed
//! by [`GitRepo::list_tree`] as [`EntryKind::Submodule`] and are omitted from
//! file reads, diffs and working changes.
//!
//! [`SkipReason::InvalidPath`]: crate::SkipReason::InvalidPath

mod diff;
mod status;
mod task;
mod tree;

use std::path::{Path, PathBuf};

use gix::bstr::ByteSlice;
use gix::hash::ObjectId;
use knowell_core::{RepoPath, TrackTarget};
use knowell_secrets::ExclusionPolicy;
use knowell_secrets::scan::redact;
use serde::{Deserialize, Serialize};

pub use task::{PathPattern, PatternError, TaskGrouping, TaskMember, TaskView, group_task_views};

use crate::fs::{WalkOptions, WalkReport};

/// Errors from git access. Messages name refs, ids and paths, never file
/// content; text taken from the underlying git library is passed through
/// secret redaction first.
#[derive(Debug, thiserror::Error)]
pub enum GitError {
    /// `path` is not the root of a git repository or worktree.
    #[error("`{}` is not a git repository or worktree root: {message}", path.display())]
    NotARepository {
        /// The path that was opened.
        path: PathBuf,
        /// Why opening failed.
        message: String,
    },
    /// The ref a track target names does not exist. No other ref is used
    /// in its place.
    #[error(
        "track target `{target}` does not exist in this repository; no other ref is used in its place"
    )]
    RefNotFound {
        /// The requested target.
        target: TrackTarget,
    },
    /// The worktree's `HEAD` names a branch that has no commit yet.
    #[error("HEAD points to `{branch}`, which has no commits yet")]
    UnbornHead {
        /// Full name of the unborn branch, e.g. `refs/heads/main`.
        branch: String,
    },
    /// The commit id is well-formed but the object does not exist.
    #[error("commit `{id}` does not exist in this repository")]
    CommitNotFound {
        /// The requested id.
        id: String,
    },
    /// The target exists but is not, and does not peel to, a commit.
    #[error("`{target}` points to a {kind}, not a commit")]
    NotACommit {
        /// The requested target or id.
        target: String,
        /// Object kind found instead (`tree`, `blob`, `tag`).
        kind: String,
    },
    /// The text is not a full object id of this repository's hash kind.
    #[error("`{id}` is not a full {expected} object id")]
    InvalidObjectId {
        /// The rejected text (truncated to 80 characters).
        id: String,
        /// Expected hash kind, e.g. `sha1`.
        expected: String,
    },
    /// A path requested from a commit is missing there or is a directory.
    #[error("`{path}` is not a file in commit `{commit}`")]
    PathNotFound {
        /// The commit that was read.
        commit: String,
        /// The requested path.
        path: RepoPath,
    },
    /// The repository has no working tree (it is bare).
    #[error("repository at `{}` has no working tree", path.display())]
    NoWorktree {
        /// Git directory of the bare repository.
        path: PathBuf,
    },
    /// A git operation failed (corrupt objects, unreadable refs, I/O).
    #[error("git {operation} failed: {message}")]
    Git {
        /// What was being done, e.g. `tree diff`.
        operation: &'static str,
        /// Redacted description from the git library.
        message: String,
    },
}

/// Renders an error and its causes (at most four levels), redacted.
fn describe(error: &(dyn std::error::Error + 'static)) -> String {
    let mut parts: Vec<String> = vec![error.to_string()];
    let mut source = error.source();
    while let Some(cause) = source {
        if parts.len() >= 4 {
            break;
        }
        let text = cause.to_string();
        if !parts.contains(&text) {
            parts.push(text);
        }
        source = cause.source();
    }
    redact(&parts.join(": ")).text
}

fn failed(operation: &'static str) -> impl FnOnce(gix::Error) -> GitError {
    move |error| GitError::Git {
        operation,
        message: describe(&error),
    }
}

/// What a resolved [`TrackTarget`] turned out to be.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ResolvedKind {
    /// A local branch (`refs/heads/…`).
    LocalBranch,
    /// A remote-tracking branch (`refs/remotes/…`).
    RemoteBranch,
    /// A tag (`refs/tags/…`), peeled through annotated tags to the commit.
    Tag,
    /// A full commit id that exists in the object database.
    Commit,
    /// A worktree whose `HEAD` is on a branch.
    WorktreeBranch,
    /// A worktree with a detached `HEAD`.
    WorktreeDetached,
}

/// The commit a [`TrackTarget`] currently points to.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ResolvedTarget {
    /// Full commit id, lowercase hex.
    pub commit: String,
    /// Fully qualified ref the target was resolved through
    /// (`refs/heads/dev`, `refs/tags/v1`); for a worktree on a branch the
    /// branch `HEAD` points to. `None` for commit ids and detached `HEAD`s.
    pub reference: Option<String>,
    /// How the target was resolved.
    pub kind: ResolvedKind,
}

/// Kind of a non-directory tree entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EntryKind {
    /// A regular file (mode `100644`).
    File,
    /// An executable file (mode `100755`).
    Executable,
    /// A symbolic link; the blob holds the link target.
    Symlink,
    /// A submodule (gitlink); the id is a commit of another repository.
    Submodule,
}

/// One non-directory entry of a commit's tree.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct TreeEntry {
    /// Path from the repository root.
    pub path: RepoPath,
    /// Object id (lowercase hex) of the blob; for a submodule, the commit id
    /// it pins.
    pub blob: String,
    /// Entry kind.
    pub mode: EntryKind,
}

/// One changed path between two trees, or between `HEAD` and a worktree.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Change {
    /// The path exists only on the new side.
    Added(RepoPath),
    /// The path exists on both sides with different content or mode.
    Modified(RepoPath),
    /// The path exists only on the old side.
    Deleted(RepoPath),
    /// Content moved from `from` to `to` (exact or similar content).
    Renamed {
        /// Old path.
        from: RepoPath,
        /// New path.
        to: RepoPath,
        /// Content similarity in percent, `100` for identical content.
        similarity: u8,
    },
}

impl Change {
    /// The path on the new side (the deleted path for [`Change::Deleted`]).
    pub fn path(&self) -> &RepoPath {
        match self {
            Change::Added(p) | Change::Modified(p) | Change::Deleted(p) => p,
            Change::Renamed { to, .. } => to,
        }
    }

    /// The path on the old side, if the change has one that differs from
    /// [`Change::path`] (only renames).
    pub fn old_path(&self) -> Option<&RepoPath> {
        match self {
            Change::Renamed { from, .. } => Some(from),
            _ => None,
        }
    }
}

/// Stable order: by new path, then by old path.
fn sort_changes(changes: &mut [Change]) {
    changes.sort_by(|a, b| {
        a.path()
            .cmp(b.path())
            .then_with(|| a.old_path().cmp(&b.old_path()))
    });
}

/// A working tree attached to a repository.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct WorktreeInfo {
    /// Root directory of the worktree, as recorded by git.
    pub path: PathBuf,
    /// Commit `HEAD` resolves to; `None` while `HEAD` is unborn.
    pub head_commit: Option<String>,
    /// Short name of the checked-out branch (`feature/payment`); `None` for
    /// a detached `HEAD`. A `HEAD` pointing outside `refs/heads/` keeps its
    /// full ref name.
    pub branch: Option<String>,
    /// Whether this is the repository's main worktree (not a linked one).
    pub is_main: bool,
    /// Whether git considers the worktree prunable: its directory is gone.
    pub prunable: bool,
}

/// An opened repository or worktree. Cheap to share: `Send + Sync`, and
/// every method opens a short-lived thread-local handle.
#[derive(Clone)]
pub struct GitRepo {
    repo: gix::ThreadSafeRepository,
    git_dir: PathBuf,
    common_dir: PathBuf,
    workdir: Option<PathBuf>,
}

impl std::fmt::Debug for GitRepo {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GitRepo")
            .field("git_dir", &self.git_dir)
            .field("workdir", &self.workdir)
            .finish_non_exhaustive()
    }
}

impl GitRepo {
    /// Opens the repository whose worktree root (or git directory) is
    /// `path`, honouring the user's git configuration (system, global and
    /// repository files) like `git` itself does. A linked worktree root
    /// opens that worktree: its own `HEAD` and index.
    ///
    /// `path` must be the root: no upward search is done, so a subdirectory
    /// of a checkout is rejected rather than silently opening its parent.
    ///
    /// # Errors
    /// [`GitError::NotARepository`].
    pub fn open(path: &Path) -> Result<Self, GitError> {
        Self::open_with(path, gix::open::Options::default())
    }

    /// Like [`GitRepo::open`], but reads only the repository's own
    /// configuration (no system or global files, no environment), so the
    /// results do not depend on the machine. Global excludes
    /// (`core.excludesFile`) are then not applied either.
    ///
    /// # Errors
    /// [`GitError::NotARepository`].
    pub fn open_isolated(path: &Path) -> Result<Self, GitError> {
        Self::open_with(path, gix::open::Options::isolated())
    }

    fn open_with(path: &Path, options: gix::open::Options) -> Result<Self, GitError> {
        let repo = gix::open_opts(path, options).map_err(|e| GitError::NotARepository {
            path: path.to_path_buf(),
            message: describe(&e),
        })?;
        Ok(Self {
            git_dir: repo.git_dir().to_path_buf(),
            common_dir: repo.common_dir().to_path_buf(),
            workdir: repo.workdir().map(Path::to_path_buf),
            repo: repo.into_sync(),
        })
    }

    pub(crate) fn local(&self) -> gix::Repository {
        self.repo.to_thread_local()
    }

    /// The worktree root, `None` for a bare repository.
    pub fn workdir(&self) -> Option<&Path> {
        self.workdir.as_deref()
    }

    /// The git directory of this worktree (for a linked worktree, its
    /// private directory below the common one).
    pub fn git_dir(&self) -> &Path {
        &self.git_dir
    }

    /// The directory shared by all worktrees (objects, refs, config).
    pub fn common_dir(&self) -> &Path {
        &self.common_dir
    }

    /// Whether this is a linked worktree (`git worktree add`).
    pub fn is_linked_worktree(&self) -> bool {
        self.git_dir != self.common_dir
    }

    /// Resolves `target` to the commit it points to right now.
    ///
    /// Branches, remote-tracking branches and tags must exist under exactly
    /// their full ref name; tags are peeled through annotated tag objects; a
    /// commit id must exist and be a commit; `worktree` follows this
    /// worktree's `HEAD` (attached or detached).
    ///
    /// # Errors
    /// [`GitError::RefNotFound`] for a missing ref (never substituted),
    /// [`GitError::CommitNotFound`], [`GitError::NotACommit`],
    /// [`GitError::InvalidObjectId`], [`GitError::UnbornHead`],
    /// [`GitError::Git`].
    pub fn resolve(&self, target: &TrackTarget) -> Result<ResolvedTarget, GitError> {
        let repo = self.local();
        let (kind, full_ref) = match target {
            TrackTarget::Branch(_) => (ResolvedKind::LocalBranch, target.full_ref()),
            TrackTarget::Remote { .. } => (ResolvedKind::RemoteBranch, target.full_ref()),
            TrackTarget::Tag(_) => (ResolvedKind::Tag, target.full_ref()),
            TrackTarget::Commit(hex) => {
                let id = parse_id(&repo, hex)?;
                require_commit(&repo, id, hex)?;
                return Ok(ResolvedTarget {
                    commit: id.to_string(),
                    reference: None,
                    kind: ResolvedKind::Commit,
                });
            }
            TrackTarget::WorktreeHead => return resolve_head(&repo),
        };
        let not_found = || GitError::RefNotFound {
            target: target.clone(),
        };
        let full_ref = full_ref.ok_or_else(not_found)?;
        let mut reference = repo
            .try_find_reference(full_ref.as_str())
            .map_err(failed("ref lookup"))?
            .ok_or_else(not_found)?;
        // `try_find_reference` also tries DWIM expansions; only the exact
        // full name counts, anything else would be a substitute.
        if reference.name().as_bstr() != full_ref.as_bytes().as_bstr() {
            return Err(not_found());
        }
        let id = reference
            .peel_to_id()
            .map_err(failed("ref peeling"))?
            .detach();
        require_commit(&repo, id, &target.to_string())?;
        Ok(ResolvedTarget {
            commit: id.to_string(),
            reference: Some(full_ref),
            kind,
        })
    }

    /// Lists every non-directory entry of `commit`'s tree, sorted by path.
    ///
    /// Entries whose name is not a valid [`RepoPath`] are omitted (see the
    /// module docs); [`GitRepo::walk_commit`] reports them.
    ///
    /// # Errors
    /// Invalid or missing commit ids, or unreadable tree objects.
    pub fn list_tree(&self, commit: &str) -> Result<Vec<TreeEntry>, GitError> {
        let repo = self.local();
        let tree_id = commit_tree(&repo, commit)?;
        let mut entries = Vec::new();
        tree::visit(&repo, tree_id, |raw, node| {
            let tree::Node::Entry(kind, id) = node else {
                return true;
            };
            let mode = match kind {
                gix::objs::tree::EntryKind::Blob => EntryKind::File,
                gix::objs::tree::EntryKind::BlobExecutable => EntryKind::Executable,
                gix::objs::tree::EntryKind::Link => EntryKind::Symlink,
                gix::objs::tree::EntryKind::Commit => EntryKind::Submodule,
                gix::objs::tree::EntryKind::Tree => return true,
            };
            if let Some(path) = tree::repo_path(raw) {
                entries.push(TreeEntry {
                    path,
                    blob: id.to_string(),
                    mode,
                });
            }
            true
        })?;
        entries.sort_by(|a, b| a.path.cmp(&b.path));
        Ok(entries)
    }

    /// Reads every file of `commit` from git objects, exactly like
    /// [`crate::walk`] reads a checkout, without touching any working tree.
    ///
    /// For every entry, in order: path exclusion (`policy`; excluded
    /// directories are pruned and reported once, `.git` / `.knowell`
    /// internals never) **before the blob is read**; the size limit from
    /// the object header **before reading**; then the shared binary /
    /// UTF-8 / redaction pipeline. The [`ContentHash`] covers the blob's
    /// original bytes (git's canonical form, i.e. after any clean filter or
    /// line-ending conversion, which can differ from the checked-out file).
    ///
    /// Symbolic links are reported as [`SkipReason::Symlink`] and never
    /// followed; submodules are omitted; blobs missing from the object
    /// database (partial clones) are [`SkipReason::Unreadable`] with
    /// `error_kind` `NotFound`. Both lists are sorted by path.
    ///
    /// # Errors
    /// Invalid or missing commit ids, or unreadable tree objects.
    ///
    /// [`ContentHash`]: knowell_core::ContentHash
    /// [`SkipReason::Symlink`]: crate::SkipReason::Symlink
    /// [`SkipReason::Unreadable`]: crate::SkipReason::Unreadable
    pub fn walk_commit(
        &self,
        commit: &str,
        policy: &ExclusionPolicy,
        options: &WalkOptions,
    ) -> Result<WalkReport, GitError> {
        let repo = self.local();
        let tree_id = commit_tree(&repo, commit)?;
        tree::walk(&repo, tree_id, policy, options)
    }

    /// Reads the given files of `commit` (for example the added and
    /// modified paths of a [`GitRepo::diff`]) with the same rules as
    /// [`GitRepo::walk_commit`]. Excluded paths are reported, never looked
    /// up; duplicates are read once; submodules are omitted.
    ///
    /// # Errors
    /// [`GitError::PathNotFound`] if a non-excluded path is missing from the
    /// commit or is a directory; invalid or missing commit ids.
    pub fn read_commit_files(
        &self,
        commit: &str,
        paths: &[RepoPath],
        policy: &ExclusionPolicy,
        options: &WalkOptions,
    ) -> Result<WalkReport, GitError> {
        let repo = self.local();
        let tree_id = commit_tree(&repo, commit)?;
        tree::read_paths(&repo, commit, tree_id, paths, policy, options)
    }

    /// Reads one file of `commit` with the same rules as
    /// [`GitRepo::walk_commit`]: path exclusion before the tree is even
    /// looked up, the size limit from the object header before the blob is
    /// read, then the shared binary / UTF-8 / redaction pipeline (the same
    /// one [`crate::fs::read_file`] uses for a checkout).
    ///
    /// Unlike [`GitRepo::read_commit_files`], a path that is absent from the
    /// commit, a directory or a submodule is [`FileRead::Missing`] rather
    /// than an error; a symbolic link is [`SkipReason::Symlink`].
    ///
    /// # Errors
    /// Invalid or missing commit ids, or unreadable tree objects.
    ///
    /// [`FileRead::Missing`]: crate::FileRead::Missing
    /// [`SkipReason::Symlink`]: crate::SkipReason::Symlink
    pub fn read_commit_file(
        &self,
        commit: &str,
        path: &RepoPath,
        policy: &ExclusionPolicy,
        options: &WalkOptions,
    ) -> Result<crate::fs::FileRead, GitError> {
        let repo = self.local();
        let tree_id = commit_tree(&repo, commit)?;
        tree::read_one(&repo, tree_id, path, policy, options)
    }

    /// The changes between the regular, built-in-policy-permitted files of
    /// `old` and `new`, sorted by repository-relative path.
    ///
    /// Rename tracking uses git's defaults explicitly (not the user's
    /// `diff.renames`): a deleted and an added file with at least 50 %
    /// similar content are reported as [`Change::Renamed`]; copies are not
    /// tracked. Mode-only changes (for example the executable bit) are
    /// [`Change::Modified`]. Directories never appear, only the files in
    /// them. This is [`GitRepo::diff_scoped`] with no project root, the
    /// built-in exclusion policy and default [`WalkOptions`] size limit.
    /// Similarity reads permitted blobs only; attributes, external filters,
    /// text conversion and working-tree files are never read.
    ///
    /// # Errors
    /// Invalid or missing commit ids, or unreadable objects.
    pub fn diff(&self, old: &str, new: &str) -> Result<Vec<Change>, GitError> {
        self.diff_scoped(
            old,
            new,
            None,
            &ExclusionPolicy::builtin(),
            &WalkOptions::default(),
        )
    }

    /// Changes between permitted regular files of two commits, sorted by
    /// repository-relative path. `root`, when present, selects a project
    /// subtree; `policy` matches paths relative to that project root.
    ///
    /// Both trees are filtered by metadata before rename similarity can
    /// access a blob. Moves across the root or exclusion boundary produce
    /// only the permitted addition or deletion. Exact renames retain 100 %
    /// similarity; edited renames require at least 50 %. Inexact matching
    /// reads only blobs no larger than `options.max_file_bytes` (bytes),
    /// and is disabled when that limit is zero. Symlinks and gitlinks are
    /// omitted. No attributes, worktree files, external filters or text
    /// conversion are consulted; temporary filtered trees exist only in memory.
    ///
    /// # Errors
    /// Invalid or missing commit ids, unreadable tree objects or permitted
    /// blobs needed for similarity matching.
    pub fn diff_scoped(
        &self,
        old: &str,
        new: &str,
        root: Option<&RepoPath>,
        policy: &ExclusionPolicy,
        options: &WalkOptions,
    ) -> Result<Vec<Change>, GitError> {
        let old = self.list_tree(old)?;
        let new = self.list_tree(new)?;
        let repo = self.local().with_object_memory();
        diff::scoped(&repo, &old, &new, root, policy, options)
    }

    /// Whether `ancestor` is reachable from `descendant` (a commit is its
    /// own ancestor).
    ///
    /// This is the fast-forward test for incremental indexing: when the
    /// previously indexed commit is **not** an ancestor of the new one, the
    /// tracked ref was force-pushed, rebased or reset, and history-based
    /// assumptions from the old view must be dropped (see the module docs).
    ///
    /// # Errors
    /// Invalid or missing commit ids, or unreadable commits.
    pub fn is_ancestor(&self, ancestor: &str, descendant: &str) -> Result<bool, GitError> {
        let repo = self.local();
        let a = parse_commit(&repo, ancestor)?;
        let d = parse_commit(&repo, descendant)?;
        if a == d {
            return Ok(true);
        }
        let bases = repo
            .merge_bases_many(a, &[d])
            .map_err(failed("merge-base"))?;
        Ok(bases.iter().any(|id| id.detach() == a))
    }

    /// The best common ancestor of `a` and `b`, `None` when the histories
    /// are unrelated. With several equally good bases, git's first choice
    /// is returned.
    ///
    /// # Errors
    /// Invalid or missing commit ids, or unreadable commits.
    pub fn merge_base(&self, a: &str, b: &str) -> Result<Option<String>, GitError> {
        let repo = self.local();
        let a = parse_commit(&repo, a)?;
        let b = parse_commit(&repo, b)?;
        if a == b {
            return Ok(Some(a.to_string()));
        }
        let bases = repo
            .merge_bases_many(a, &[b])
            .map_err(failed("merge-base"))?;
        Ok(bases.first().map(|id| id.detach().to_string()))
    }

    /// All worktrees of the repository: the main worktree first (absent for
    /// a bare repository), then linked worktrees sorted by path. Works the
    /// same when opened from any of them. Existing paths are canonical
    /// absolute paths; missing worktrees retain their registered paths.
    ///
    /// # Errors
    /// [`GitError::Git`] if the worktree metadata cannot be read.
    pub fn worktrees(&self) -> Result<Vec<WorktreeInfo>, GitError> {
        let repo = self.local();
        let main = repo.main_repo().map_err(failed("main repository lookup"))?;
        let mut out = Vec::new();
        if let Some(workdir) = main.workdir() {
            let (head_commit, branch) = head_state(&main)?;
            out.push(WorktreeInfo {
                path: worktree_path(workdir)?,
                head_commit,
                branch,
                is_main: true,
                prunable: !workdir.is_dir(),
            });
        }
        let proxies = main.worktrees().map_err(|e| GitError::Git {
            operation: "worktree listing",
            message: describe(&e),
        })?;
        let mut linked = Vec::with_capacity(proxies.len());
        for proxy in proxies {
            let prunable = proxy.is_prunable();
            let Ok(path) = proxy.base() else {
                // No readable `gitdir` file: git itself cannot locate this
                // worktree any more (`git worktree prune` removes it).
                continue;
            };
            let wt = proxy
                .into_repo_with_possibly_inaccessible_worktree()
                .map_err(failed("worktree open"))?;
            let (head_commit, branch) = head_state(&wt)?;
            linked.push(WorktreeInfo {
                path: worktree_path(&path)?,
                head_commit,
                branch,
                is_main: false,
                prunable,
            });
        }
        linked.sort_by(|a, b| a.path.cmp(&b.path));
        out.extend(linked);
        Ok(out)
    }

    /// Uncommitted changes of this worktree relative to its own `HEAD`:
    /// modified, added (untracked and not ignored, or staged) and deleted
    /// files, staged or not. This is the "saved but not committed" state
    /// that feeds a personal overlay.
    ///
    /// Paths excluded by `policy` are filtered before status may hash content;
    /// ignored files, submodules and empty directories do not appear;
    /// renames appear as a deletion plus an addition. Staged changes that
    /// a later worktree edit undoes may be reported as `Modified`
    /// (conservative). Nothing is written, not even the index stat cache.
    /// An unborn `HEAD` compares against the empty tree.
    ///
    /// # Errors
    /// [`GitError::NoWorktree`] for a bare repository; [`GitError::Git`].
    pub fn working_changes(&self, policy: &ExclusionPolicy) -> Result<Vec<Change>, GitError> {
        self.working_changes_scoped(None, policy, &WalkOptions::default())
    }

    /// Uncommitted changes restricted to `root` and `policy` before any
    /// content hashing. The policy matches project-relative paths beneath
    /// `root`; returned paths remain repository-relative. `options` carries
    /// the maximum permitted file size in bytes. Nothing is written.
    ///
    /// # Errors
    /// [`GitError::NoWorktree`] for a bare repository; [`GitError::Git`].
    pub fn working_changes_scoped(
        &self,
        root: Option<&RepoPath>,
        policy: &ExclusionPolicy,
        options: &WalkOptions,
    ) -> Result<Vec<Change>, GitError> {
        let repo = self.local();
        if repo.workdir().is_none() {
            return Err(GitError::NoWorktree {
                path: self.git_dir.clone(),
            });
        }
        let mut changes = status::working_changes_scoped(&repo, root, policy, options)?;
        sort_changes(&mut changes);
        Ok(changes)
    }
}

/// Opens the worktree at `worktree` (main or linked, honouring the user's
/// git configuration) and returns [`GitRepo::working_changes`].
///
/// # Errors
/// See [`GitRepo::open`] and [`GitRepo::working_changes`].
pub fn working_changes(worktree: &Path, policy: &ExclusionPolicy) -> Result<Vec<Change>, GitError> {
    GitRepo::open(worktree)?.working_changes(policy)
}

fn worktree_path(path: &Path) -> Result<PathBuf, GitError> {
    // Git can register long Windows paths while TEMP uses their 8.3 aliases.
    // Canonical paths keep a worktree's identity independent of the entry point.
    match std::fs::canonicalize(path) {
        Ok(path) => Ok(path),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(path.to_path_buf()),
        Err(error) => Err(GitError::Git {
            operation: "worktree path resolution",
            message: describe(&error),
        }),
    }
}

fn hash_name(kind: gix::hash::Kind) -> String {
    format!("{kind:?}").to_lowercase()
}

/// Parses a full hex id of the repository's hash kind.
fn parse_id(repo: &gix::Repository, text: &str) -> Result<ObjectId, GitError> {
    let expected = repo.object_hash();
    let invalid = || GitError::InvalidObjectId {
        id: text.chars().take(80).collect(),
        expected: hash_name(expected),
    };
    if text.len() != expected.len_in_hex() {
        return Err(invalid());
    }
    let id = ObjectId::from_hex(text.as_bytes()).map_err(|_| invalid())?;
    if id.kind() != expected {
        return Err(invalid());
    }
    Ok(id)
}

/// Fails unless `id` exists and is a commit.
fn require_commit(repo: &gix::Repository, id: ObjectId, target: &str) -> Result<(), GitError> {
    let header = repo
        .try_find_header(id)
        .map_err(failed("object lookup"))?
        .ok_or_else(|| GitError::CommitNotFound { id: id.to_string() })?;
    let kind = header.kind();
    if kind != gix::objs::Kind::Commit {
        return Err(GitError::NotACommit {
            target: target.to_owned(),
            kind: kind.to_string(),
        });
    }
    Ok(())
}

/// Parses `text` and checks that it names an existing commit.
fn parse_commit(repo: &gix::Repository, text: &str) -> Result<ObjectId, GitError> {
    let id = parse_id(repo, text)?;
    require_commit(repo, id, text)?;
    Ok(id)
}

/// The root tree id of commit `text`.
fn commit_tree(repo: &gix::Repository, text: &str) -> Result<ObjectId, GitError> {
    let id = parse_commit(repo, text)?;
    let commit = repo.find_commit(id).map_err(failed("commit lookup"))?;
    let tree = commit.tree_id().map_err(|e| GitError::Git {
        operation: "commit decoding",
        message: describe(&e.into_error()),
    })?;
    Ok(tree.detach())
}

fn resolve_head(repo: &gix::Repository) -> Result<ResolvedTarget, GitError> {
    let head = repo.head().map_err(failed("HEAD lookup"))?;
    let attached = match &head.kind {
        gix::head::Kind::Unborn(name) => {
            return Err(GitError::UnbornHead {
                branch: name.as_bstr().to_str_lossy().into_owned(),
            });
        }
        gix::head::Kind::Detached { .. } => false,
        gix::head::Kind::Symbolic(_) => true,
    };
    if !attached {
        let id = head
            .into_peeled_id()
            .map_err(failed("HEAD peeling"))?
            .detach();
        require_commit(repo, id, "HEAD")?;
        return Ok(ResolvedTarget {
            commit: id.to_string(),
            reference: None,
            kind: ResolvedKind::WorktreeDetached,
        });
    }
    let mut reference = head.try_into_referent().ok_or_else(|| GitError::Git {
        operation: "HEAD lookup",
        message: "HEAD changed while it was read".to_owned(),
    })?;
    let name = reference.name().as_bstr().to_str_lossy().into_owned();
    let id = reference
        .peel_to_id()
        .map_err(failed("HEAD peeling"))?
        .detach();
    require_commit(repo, id, "HEAD")?;
    Ok(ResolvedTarget {
        commit: id.to_string(),
        reference: Some(name),
        kind: ResolvedKind::WorktreeBranch,
    })
}

/// `(head commit, branch short name)` of an opened worktree.
fn head_state(repo: &gix::Repository) -> Result<(Option<String>, Option<String>), GitError> {
    let head = repo.head().map_err(failed("HEAD lookup"))?;
    let branch = head.referent_name().map(|name| {
        let full = name.as_bstr();
        full.strip_prefix(b"refs/heads/")
            .unwrap_or(full)
            .to_str_lossy()
            .into_owned()
    });
    let commit = match head.kind {
        gix::head::Kind::Unborn(_) => None,
        _ => match head.try_into_peeled_id() {
            Ok(id) => id.map(|id| id.detach().to_string()),
            Err(e) => return Err(failed("HEAD peeling")(e)),
        },
    };
    Ok((commit, branch))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(s: &str) -> RepoPath {
        RepoPath::new(s).unwrap()
    }

    #[test]
    fn change_paths_and_order() {
        let mut changes = vec![
            Change::Renamed {
                from: p("z"),
                to: p("b"),
                similarity: 100,
            },
            Change::Deleted(p("c")),
            Change::Added(p("a")),
        ];
        sort_changes(&mut changes);
        let paths: Vec<&str> = changes.iter().map(|c| c.path().as_str()).collect();
        assert_eq!(paths, ["a", "b", "c"]);
        assert_eq!(changes[1].old_path().map(RepoPath::as_str), Some("z"));
        assert_eq!(changes[0].old_path(), None);
    }

    #[test]
    fn error_messages_are_lowercase_and_redacted() {
        let token = format!("ghp_{}", "FAKE".repeat(9));
        let error = std::io::Error::other(format!("bad header {token}"));
        let text = describe(&error);
        assert!(!text.contains(&token));
        assert!(text.contains("[REDACTED:github_token]"));
        let e = GitError::RefNotFound {
            target: "branch:dev".parse().unwrap(),
        };
        assert!(e.to_string().starts_with("track target `branch:dev`"));
    }

    #[test]
    fn git_repo_is_send_and_sync() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<GitRepo>();
    }
}
