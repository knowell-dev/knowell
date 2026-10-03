//! Personal overlays: a worktree's own state on top of a shared view.
//!
//! An overlay is the difference between a view's active generation and a
//! worktree's real state — its own `HEAD` (committed but not yet indexed,
//! or another branch) plus saved, uncommitted changes. It lives in memory
//! only: the shared store and the view's lexical index are read, never
//! written, so a feature branch's edits never leak into the shared view.
//! Queries use [`Overlay::search`] and drop base-view evidence for
//! [`Overlay::shadowed_paths`].

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::AtomicBool;

use knowell_core::{ContentHash, Name, RepoPath, TrackTarget};
use knowell_embed::Embedder;
use knowell_lexical::{LexicalDoc, LexicalHit, LexicalIndex};
use knowell_parse::ParsedFile;
use knowell_source::git::{Change, GitError};
use knowell_source::{FileRead, SourceFile, WalkOptions};
use knowell_store::views::{self, GenerationPin};
use knowell_store::{SourceKind, ViewId};

use crate::analyze::{AnalyseOptions, analyse};
use crate::context::ViewContext;
use crate::error::IndexError;
use crate::indexer::Inner;
use crate::pipeline::files_map;
use crate::plan::open_repo;
use crate::status::ProgressKind;

/// One file of an overlay: its worktree version, analysed.
#[derive(Debug, Clone)]
pub struct OverlayFile {
    /// Project-relative path.
    pub path: RepoPath,
    /// Hash of the original bytes.
    pub content_hash: ContentHash,
    /// Redacted text.
    pub text: Arc<str>,
    /// Symbols, imports and degradation of this version.
    pub parsed: Arc<ParsedFile>,
}

/// A worktree's personal layer over one view. See the module docs.
pub struct Overlay {
    view: ViewId,
    project: Name,
    worktree: PathBuf,
    base_generation: Option<i64>,
    base_commit: Option<String>,
    head_commit: Option<String>,
    files: BTreeMap<RepoPath, OverlayFile>,
    deleted: BTreeSet<RepoPath>,
    lexical: LexicalIndex,
}

impl fmt::Debug for Overlay {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Overlay")
            .field("view", &self.view)
            .field("project", &self.project)
            .field("worktree", &self.worktree)
            .field("base_generation", &self.base_generation)
            .field("head_commit", &self.head_commit)
            .field("files", &self.files.keys().collect::<Vec<_>>())
            .field("deleted", &self.deleted)
            .finish_non_exhaustive()
    }
}

impl Overlay {
    /// The base view.
    pub fn view(&self) -> ViewId {
        self.view
    }

    /// The project.
    pub fn project(&self) -> &Name {
        &self.project
    }

    /// The worktree root the overlay was built from.
    pub fn worktree(&self) -> &Path {
        &self.worktree
    }

    /// The base view's generation the overlay was computed against.
    pub fn base_generation(&self) -> Option<i64> {
        self.base_generation
    }

    /// The base view's commit the overlay was computed against.
    pub fn base_commit(&self) -> Option<&str> {
        self.base_commit.as_deref()
    }

    /// The commit the worktree's `HEAD` pointed to.
    pub fn head_commit(&self) -> Option<&str> {
        self.head_commit.as_deref()
    }

    /// Added or changed files, by path.
    pub fn files(&self) -> impl Iterator<Item = &OverlayFile> {
        self.files.values()
    }

    /// One added or changed file.
    pub fn file(&self, path: &RepoPath) -> Option<&OverlayFile> {
        self.files.get(path)
    }

    /// Files of the base view that the worktree deleted.
    pub fn deleted(&self) -> &BTreeSet<RepoPath> {
        &self.deleted
    }

    /// Every base-view path whose evidence is stale for this worktree:
    /// changed, added and deleted paths.
    pub fn shadowed_paths(&self) -> BTreeSet<RepoPath> {
        self.files
            .keys()
            .chain(self.deleted.iter())
            .cloned()
            .collect()
    }

    /// Whether the worktree matches the base view exactly.
    pub fn is_empty(&self) -> bool {
        self.files.is_empty() && self.deleted.is_empty()
    }

    /// BM25 search over the overlay's files only (ids are paths).
    ///
    /// # Errors
    /// Lexical index errors.
    pub fn search(&self, query: &str, limit: usize) -> Result<Vec<LexicalHit>, IndexError> {
        Ok(self.lexical.search(query, limit)?)
    }
}

/// What the worktree holds for each differing path.
enum Version {
    File(SourceFile),
    Deleted,
}

struct Collected {
    head: Option<String>,
    versions: BTreeMap<RepoPath, Version>,
}

/// Reads the worktree's differences from `base_commit` (blocking).
fn collect(
    ctx: &ViewContext,
    worktree: &Path,
    base_commit: Option<&str>,
    mode: crate::config::GitConfigMode,
    max_bytes: u64,
) -> Result<Collected, IndexError> {
    let repo = open_repo(worktree, mode)?;
    let options = WalkOptions {
        max_file_bytes: max_bytes,
        respect_gitignore: false,
        follow_symlinks: false,
    };
    let head = match repo.resolve(&TrackTarget::WorktreeHead) {
        Ok(resolved) => Some(resolved.commit),
        Err(GitError::UnbornHead { .. }) => None,
        Err(e) => return Err(e.into()),
    };
    let mut versions: BTreeMap<RepoPath, Version> = BTreeMap::new();
    // Committed differences between the view and the worktree's HEAD.
    if let (Some(base), Some(head)) = (base_commit, head.as_deref())
        && base != head
    {
        let mut read = Vec::new();
        for change in repo.diff_scoped(base, head, ctx.root.as_ref(), &ctx.policy, &options)? {
            if let Some(old) = change.old_path().and_then(|p| ctx.to_project_path(p)) {
                versions.insert(old, Version::Deleted);
            }
            let Some(path) = ctx.to_project_path(change.path()) else {
                continue;
            };
            match change {
                Change::Deleted(_) => {
                    versions.insert(path, Version::Deleted);
                }
                Change::Added(repo_path)
                | Change::Modified(repo_path)
                | Change::Renamed { to: repo_path, .. } => {
                    if ctx.policy.check(&path).is_none() {
                        read.push(repo_path);
                    }
                }
            }
        }
        if !read.is_empty() {
            let report = repo.read_commit_files(head, &read, &ctx.policy, &options)?;
            for file in report.files {
                if let Some(path) = ctx.to_project_path(&file.path) {
                    versions.insert(path.clone(), Version::File(SourceFile { path, ..file }));
                }
            }
            for skip in report.skipped {
                if let Some(path) = ctx.to_project_path(&skip.path) {
                    // Present but not indexable: shadow the base version.
                    versions.insert(path, Version::Deleted);
                }
            }
        }
    }
    // Saved but uncommitted changes, read from the working tree.
    let saved_options = WalkOptions {
        respect_gitignore: true,
        ..options.clone()
    };
    for change in repo.working_changes_scoped(ctx.root.as_ref(), &ctx.policy, &saved_options)? {
        let Some(path) = ctx.to_project_path(change.path()) else {
            continue;
        };
        if let Some(old) = change.old_path().and_then(|p| ctx.to_project_path(p))
            && ctx.policy.check(&old).is_none()
        {
            versions.insert(old, Version::Deleted);
        }
        if ctx.policy.check(&path).is_some() {
            continue;
        }
        let repo_path = change.path().clone();
        match change {
            Change::Deleted(_) => {
                versions.insert(path, Version::Deleted);
            }
            Change::Added(_) | Change::Modified(_) | Change::Renamed { .. } => {
                // The same single-file pipeline as every other read:
                // exclusion by path first, size, binary, UTF-8, redaction.
                match knowell_source::read_file(worktree, &repo_path, &ctx.policy, &options)? {
                    FileRead::File(file) => {
                        versions.insert(path.clone(), Version::File(SourceFile { path, ..file }));
                    }
                    FileRead::Missing => {
                        versions.insert(path, Version::Deleted);
                    }
                    FileRead::Skipped(reason) => {
                        // Present but not indexable: shadow the base version.
                        tracing::debug!(%path, reason = reason.as_str(), "overlay file not indexable");
                        versions.insert(path, Version::Deleted);
                    }
                }
            }
        }
    }
    Ok(Collected { head, versions })
}

impl<E: Embedder + 'static> Inner<E> {
    /// See [`crate::Indexer::build_overlay`].
    pub(crate) async fn build_overlay(
        &self,
        view: ViewId,
        worktree: &Path,
    ) -> Result<Arc<Overlay>, IndexError> {
        let ctx = self.context(view)?;
        if ctx.source_kind != SourceKind::Git {
            return Err(IndexError::Config(format!(
                "project `{}` is a plain directory; overlays need a git worktree",
                ctx.project_name
            )));
        }
        let mut conn = self.store.acquire().await?;
        let row = views::get_view(&mut conn, view)
            .await?
            .ok_or(IndexError::UnknownView(view))?;
        let base_files = match row.active_generation {
            Some(generation) => files_map(&mut conn, GenerationPin { view, generation }).await?,
            None => BTreeMap::new(),
        };
        drop(conn);
        let options = AnalyseOptions {
            chunking: self.config.chunking,
            limits: self.config.parse_limits,
            generated: self.config.content.generated,
            identifiers: false,
        };
        let mode = self.config.git_config;
        let max_bytes = self.config.limits.max_file_bytes;
        let worktree_dir = worktree.to_path_buf();
        let base_commit = row.active_commit.clone();
        let task_ctx = Arc::clone(&ctx);
        let overlay = tokio::task::spawn_blocking(move || -> Result<Overlay, IndexError> {
            let collected = collect(
                &task_ctx,
                &worktree_dir,
                base_commit.as_deref(),
                mode,
                max_bytes,
            )?;
            let cancel = AtomicBool::new(false);
            let mut files = BTreeMap::new();
            let mut deleted = BTreeSet::new();
            for (path, version) in collected.versions {
                match version {
                    Version::Deleted => {
                        if base_files.contains_key(&path) {
                            deleted.insert(path);
                        }
                    }
                    Version::File(file) => {
                        // Same content as the base view: nothing to shadow.
                        if base_files.get(&path) == Some(&file.hash) {
                            continue;
                        }
                        let text: Arc<str> = Arc::from(file.text.as_str());
                        let analysed = analyse(
                            task_ctx.project_name.as_str(),
                            &path,
                            file.hash,
                            Arc::clone(&text),
                            &options,
                            &cancel,
                        );
                        files.insert(
                            path.clone(),
                            OverlayFile {
                                path,
                                content_hash: file.hash,
                                text,
                                parsed: analysed.parsed,
                            },
                        );
                    }
                }
            }
            let lexical = LexicalIndex::create_in_ram()?;
            let mut writer = lexical.writer()?;
            for file in files.values() {
                let path = file.path.as_str();
                writer.add(LexicalDoc {
                    id: path,
                    path,
                    text: &file.text,
                })?;
            }
            writer.commit()?;
            Ok(Overlay {
                view: task_ctx.view,
                project: task_ctx.project_name.clone(),
                worktree: worktree_dir,
                base_generation: row.active_generation,
                base_commit,
                head_commit: collected.head,
                files,
                deleted,
                lexical,
            })
        })
        .await??;
        let overlay = Arc::new(overlay);
        let shadowed = overlay.files.len() + overlay.deleted.len();
        self.overlays
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(view, Arc::clone(&overlay));
        self.emit(
            &ctx,
            overlay.base_generation,
            None,
            ProgressKind::OverlayUpdated { shadowed },
        );
        Ok(overlay)
    }
}
