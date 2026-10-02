//! Workspace import: derives a `knowell.toml` from the layout of a directory
//! (submodules, language workspaces, folders of repositories).
//!
//! Nothing is guessed: a project's track target comes from `.gitmodules`
//! (`branch = x`), or from the repository's current branch when the caller
//! passes [`ImportOptions::track_current`]. Otherwise it stays `None`, the
//! rendered file carries a `TODO`, and the caller is expected to ask the user.

mod glob;
mod names;
mod render;
mod sources;

use std::collections::{BTreeSet, HashSet};
use std::fs;
use std::path::Path;
use std::str::FromStr;

use knowell_core::{Name, RepoPath, TrackTarget};

use crate::error::SetupError;

pub use render::render_toml;

/// Which file or scan produced a project.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ImportSource {
    /// `.gitmodules`.
    Gitmodules,
    /// `go.work`.
    GoWork,
    /// `pnpm-workspace.yaml`.
    PnpmWorkspace,
    /// `workspaces` in `package.json` (npm, yarn).
    PackageJson,
    /// `[workspace] members` in `Cargo.toml`.
    CargoWorkspace,
    /// A direct child directory that is a git repository.
    FolderScan,
    /// The directory itself, because it is a repository and nothing else was found.
    RootRepository,
}

impl ImportSource {
    /// Short label used in comments of the rendered file.
    pub fn label(self) -> &'static str {
        match self {
            ImportSource::Gitmodules => ".gitmodules",
            ImportSource::GoWork => "go.work",
            ImportSource::PnpmWorkspace => "pnpm-workspace.yaml",
            ImportSource::PackageJson => "package.json workspaces",
            ImportSource::CargoWorkspace => "Cargo workspace",
            ImportSource::FolderScan => "folder scan",
            ImportSource::RootRepository => "repository root",
        }
    }
}

/// Options of [`detect`].
#[derive(Debug, Clone)]
pub struct ImportOptions {
    /// Workspace name; default: the directory name, slugified.
    pub workspace_name: Option<String>,
    /// Read each repository's currently checked-out branch as its track
    /// target (`branch:<current>`). Off by default: Knowell never guesses.
    pub track_current: bool,
    /// Glob patterns (relative to the root) of folders holding git
    /// worktrees. Matches are reported as worktrees, never as projects.
    /// Default: `.worktree/*/*`.
    pub worktree_patterns: Vec<String>,
    /// Add direct child directories that are git repositories. Default: true.
    pub scan_folders: bool,
}

impl Default for ImportOptions {
    fn default() -> Self {
        Self {
            workspace_name: None,
            track_current: false,
            worktree_patterns: vec![".worktree/*/*".to_owned()],
            scan_folders: true,
        }
    }
}

/// A project the import proposes (one `[[project]]` table).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlannedProject {
    /// Unique project name.
    pub name: Name,
    /// Directory of the repository relative to the import root, `/`-separated;
    /// `.` for the root itself.
    pub path: String,
    /// Sub-directory of the repository for monorepo sub-projects.
    pub root: Option<RepoPath>,
    /// Clone URL without credentials, when known.
    pub remote: Option<String>,
    /// Ref to follow; `None` means the user has to decide.
    pub track: Option<TrackTarget>,
    /// What discovered the project.
    pub source: ImportSource,
}

/// Why a folder is reported as a worktree.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WorktreeKind {
    /// It matched one of [`ImportOptions::worktree_patterns`].
    Pattern,
    /// A direct child whose `.git` file points into another repository's
    /// `worktrees/` directory.
    LinkedWorktree,
}

/// A git worktree found next to the projects; not a project of its own.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlannedWorktree {
    /// Directory relative to the import root, `/`-separated.
    pub path: String,
    /// How it was recognised.
    pub kind: WorktreeKind,
}

/// Why a project name differs from the text it came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RenameReason {
    /// The original text was not a valid name and was slugified.
    Slugified,
    /// Another project already had the name; a numeric suffix was added.
    Collision,
}

/// A project name that was changed to be valid or unique.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Rename {
    /// Where the project lives (`path`, plus `root` when set).
    pub location: String,
    /// The name the source suggested.
    pub original: String,
    /// The name assigned.
    pub assigned: Name,
    /// Why it changed.
    pub reason: RenameReason,
}

/// Result of [`detect`]: everything needed to write and explain `knowell.toml`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImportPlan {
    /// Workspace name.
    pub workspace_name: Name,
    /// Proposed projects in discovery order (submodules, language
    /// workspaces, folder scan).
    pub projects: Vec<PlannedProject>,
    /// Worktrees found; never projects.
    pub worktrees: Vec<PlannedWorktree>,
    /// Names that were slugified or de-duplicated.
    pub renames: Vec<Rename>,
    /// Problems and caveats, in plain English.
    pub warnings: Vec<String>,
}

impl ImportPlan {
    /// Projects whose track target the user still has to choose.
    pub fn projects_needing_track(&self) -> Vec<&PlannedProject> {
        self.projects.iter().filter(|p| p.track.is_none()).collect()
    }
}

struct Candidate {
    original_name: String,
    path: String,
    root: Option<String>,
    remote: Option<String>,
    track: Option<TrackTarget>,
    source: ImportSource,
}

struct Detector<'a> {
    root: &'a Path,
    opts: &'a ImportOptions,
    candidates: Vec<Candidate>,
    seen: HashSet<(String, Option<String>)>,
    worktrees: Vec<PlannedWorktree>,
    warnings: Vec<String>,
}

/// Inspects `root` and proposes a workspace.
///
/// Reads only the well-known layout files and `.git` pointers (`HEAD` for
/// [`ImportOptions::track_current`]); never runs git and never touches the
/// network.
///
/// # Errors
///
/// Returns an error when `root` is not a readable directory.
pub fn detect(root: &Path, opts: &ImportOptions) -> Result<ImportPlan, SetupError> {
    if !root.is_dir() {
        return Err(SetupError::Read {
            path: root.to_path_buf(),
            source: std::io::Error::new(std::io::ErrorKind::NotFound, "not a directory"),
        });
    }
    let workspace_name = workspace_name(root, opts)?;
    let mut d = Detector {
        root,
        opts,
        candidates: Vec::new(),
        seen: HashSet::new(),
        worktrees: Vec::new(),
        warnings: Vec::new(),
    };
    d.find_worktrees();
    d.gitmodules();
    d.go_work();
    d.pnpm_and_package_json();
    d.cargo();
    if opts.scan_folders {
        d.scan_folders();
    }
    if d.candidates.is_empty() && is_repo(root, "") {
        let track = d.current_branch_track(".");
        d.add(Candidate {
            original_name: dir_name(root),
            path: ".".to_owned(),
            root: None,
            remote: None,
            track,
            source: ImportSource::RootRepository,
        });
    }
    Ok(d.finish(workspace_name))
}

fn dir_name(root: &Path) -> String {
    let abs = fs::canonicalize(root).ok();
    abs.as_deref()
        .unwrap_or(root)
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("workspace")
        .to_owned()
}

fn workspace_name(root: &Path, opts: &ImportOptions) -> Result<Name, SetupError> {
    let text = match &opts.workspace_name {
        Some(n) => {
            return Name::new(n.clone()).map_err(|e| SetupError::InvalidInput(e.to_string()));
        }
        None => dir_name(root),
    };
    let slug = names::slugify(&text);
    let slug = if slug == "project" {
        "workspace".to_owned()
    } else {
        slug
    };
    Name::new(slug).map_err(|e| SetupError::InvalidInput(e.to_string()))
}

fn is_repo(root: &Path, rel: &str) -> bool {
    root.join(rel).join(".git").exists()
}

/// Nearest repository containing `rel` (itself or an ancestor, up to the
/// root); `Some("")` is the root itself.
fn enclosing_repo(root: &Path, rel: &str) -> Option<String> {
    let mut current = rel.to_owned();
    loop {
        if is_repo(root, &current) {
            return Some(current);
        }
        if current.is_empty() {
            return None;
        }
        current = match current.rsplit_once('/') {
            Some((parent, _)) => parent.to_owned(),
            None => String::new(),
        };
    }
}

/// Reads `HEAD` of the repository at `repo_dir` without running git.
/// `Ok(Some(branch))`, `Ok(None)` when detached; `Err` when unreadable.
fn read_head_branch(repo_dir: &Path) -> Result<Option<String>, ()> {
    let dot_git = repo_dir.join(".git");
    let git_dir = if dot_git.is_dir() {
        dot_git
    } else {
        let pointer = read_small(&dot_git).ok_or(())?;
        let target = pointer.trim().strip_prefix("gitdir:").ok_or(())?.trim();
        let target = Path::new(target);
        if target.is_absolute() {
            target.to_path_buf()
        } else {
            repo_dir.join(target)
        }
    };
    let head = read_small(&git_dir.join("HEAD")).ok_or(())?;
    Ok(head
        .trim()
        .strip_prefix("ref: refs/heads/")
        .map(|b| b.trim().to_owned()))
}

/// Reads a small text file (git pointer files are tiny); larger files are
/// refused so a hostile repository cannot make import read huge files.
fn read_small(path: &Path) -> Option<String> {
    let meta = fs::metadata(path).ok()?;
    if meta.len() > 4096 {
        return None;
    }
    fs::read_to_string(path).ok()
}

/// Reads a layout file at the root, refusing very large ones.
fn read_layout_file(path: &Path) -> Option<String> {
    let meta = fs::metadata(path).ok()?;
    if !meta.is_file() || meta.len() > 1_048_576 {
        return None;
    }
    fs::read_to_string(path).ok()
}

impl Detector<'_> {
    fn warn(&mut self, message: impl Into<String>) {
        self.warnings.push(message.into());
    }

    fn is_worktree(&self, path: &str) -> bool {
        self.worktrees.iter().any(|w| w.path == path)
    }

    fn add(&mut self, c: Candidate) {
        if self.is_worktree(&c.path) {
            return;
        }
        if self.seen.insert((c.path.clone(), c.root.clone())) {
            self.candidates.push(c);
        }
    }

    /// Track target when the caller opted into reading the current branch.
    fn current_branch_track(&mut self, repo_rel: &str) -> Option<TrackTarget> {
        if !self.opts.track_current {
            return None;
        }
        let dir = if repo_rel == "." {
            self.root.to_path_buf()
        } else {
            self.root.join(repo_rel)
        };
        match read_head_branch(&dir) {
            Ok(Some(branch)) => match TrackTarget::from_str(&format!("branch:{branch}")) {
                Ok(t) => Some(t),
                Err(_) => {
                    self.warn(format!(
                        "`{repo_rel}`: the current branch name is not a valid track target; choose one"
                    ));
                    None
                }
            },
            Ok(None) => {
                self.warn(format!(
                    "`{repo_rel}`: HEAD is detached; choose a branch, tag or commit to track"
                ));
                None
            }
            Err(()) => {
                self.warn(format!(
                    "`{repo_rel}`: not a readable git repository; cannot read its current branch"
                ));
                None
            }
        }
    }

    fn find_worktrees(&mut self) {
        let mut found = BTreeSet::new();
        for pattern in &self.opts.worktree_patterns {
            if !glob::is_safe_pattern(pattern) {
                self.warnings
                    .push(format!("ignored unsafe worktree pattern `{pattern}`"));
                continue;
            }
            found.extend(glob::expand_dirs(self.root, pattern));
        }
        self.worktrees
            .extend(found.into_iter().map(|path| PlannedWorktree {
                path,
                kind: WorktreeKind::Pattern,
            }));
    }

    fn gitmodules(&mut self) {
        let Some(text) = read_layout_file(&self.root.join(".gitmodules")) else {
            return;
        };
        for sub in sources::parse_gitmodules(&text) {
            let Some(path) = sub.path.clone() else {
                self.warn(format!("submodule `{}` has no path; skipped", sub.name));
                continue;
            };
            let path = path.trim_end_matches('/').to_owned();
            if RepoPath::new(path.clone()).is_err() {
                self.warn(format!(
                    "submodule `{}` has an unusable path `{path}`; skipped",
                    sub.name
                ));
                continue;
            }
            if !self.root.join(&path).is_dir() {
                self.warn(format!(
                    "submodule `{}` is not checked out at `{path}`; run `git submodule update --init` before indexing",
                    sub.name
                ));
            }
            let track = match sub.branch.as_deref().map(str::trim) {
                Some(".") => {
                    self.warn(format!(
                        "submodule `{}` uses `branch = .` (same as the superproject); choose its track target",
                        sub.name
                    ));
                    None
                }
                Some(b) if !b.is_empty() => match TrackTarget::from_str(&format!("branch:{b}")) {
                    Ok(t) => Some(t),
                    Err(_) => {
                        self.warn(format!(
                            "submodule `{}` declares an invalid branch; choose its track target",
                            sub.name
                        ));
                        None
                    }
                },
                _ => self.current_branch_track(&path),
            };
            let name = sub
                .name
                .rsplit('/')
                .next()
                .filter(|s| !s.is_empty())
                .unwrap_or(&sub.name)
                .to_owned();
            self.add(Candidate {
                original_name: name,
                path,
                root: None,
                remote: sub.url.as_deref().map(sources::sanitize_remote),
                track,
                source: ImportSource::Gitmodules,
            });
        }
    }

    /// Adds a monorepo sub-project at `dir` (relative to the import root).
    fn add_member(&mut self, dir: &str, source: ImportSource) {
        let name = dir.rsplit('/').next().unwrap_or(dir).to_owned();
        match enclosing_repo(self.root, dir) {
            Some(repo) => {
                let (path, root) = if repo.is_empty() {
                    (".".to_owned(), Some(dir.to_owned()))
                } else if repo == dir {
                    (repo.clone(), None)
                } else {
                    let sub = dir
                        .strip_prefix(&format!("{repo}/"))
                        .unwrap_or(dir)
                        .to_owned();
                    (repo.clone(), Some(sub))
                };
                let track = self.current_branch_track(&path);
                self.add(Candidate {
                    original_name: name,
                    path,
                    root,
                    remote: None,
                    track,
                    source,
                });
            }
            None => {
                self.warn(format!(
                    "`{dir}` is not inside a git repository; added without a track target"
                ));
                self.add(Candidate {
                    original_name: name,
                    path: dir.to_owned(),
                    root: None,
                    remote: None,
                    track: None,
                    source,
                });
            }
        }
    }

    fn go_work(&mut self) {
        let Some(text) = read_layout_file(&self.root.join("go.work")) else {
            return;
        };
        for entry in sources::parse_go_work(&text) {
            let rel = entry.trim_start_matches("./").trim_end_matches('/');
            if rel == "." || rel.is_empty() {
                self.add_member_root(ImportSource::GoWork);
                continue;
            }
            if !glob::is_safe_pattern(rel) {
                self.warn(format!(
                    "go.work entry `{entry}` points outside the workspace root; skipped"
                ));
                continue;
            }
            if !self.root.join(rel).is_dir() {
                self.warn(format!("go.work entry `{entry}` does not exist; skipped"));
                continue;
            }
            self.add_member(rel, ImportSource::GoWork);
        }
    }

    /// The import root itself as a member (`use .`).
    fn add_member_root(&mut self, source: ImportSource) {
        if is_repo(self.root, "") {
            let track = self.current_branch_track(".");
            self.add(Candidate {
                original_name: dir_name(self.root),
                path: ".".to_owned(),
                root: None,
                remote: None,
                track,
                source,
            });
        } else {
            self.warn(
                "the workspace root is a module but not a git repository; skipped".to_owned(),
            );
        }
    }

    /// Expands member patterns (with `!` exclusions), keeping directories
    /// that contain `marker_file`.
    fn expand_members(&mut self, patterns: &[String], marker_file: &str) -> Vec<String> {
        let (negative, positive): (Vec<&String>, Vec<&String>) =
            patterns.iter().partition(|p| p.starts_with('!'));
        let negative: Vec<&str> = negative
            .iter()
            .filter_map(|p| p.strip_prefix('!'))
            .collect();
        let mut out = BTreeSet::new();
        for pattern in positive {
            if !glob::is_safe_pattern(pattern) {
                self.warn(format!(
                    "workspace pattern `{pattern}` points outside the root; skipped"
                ));
                continue;
            }
            for dir in glob::expand_dirs(self.root, pattern) {
                let excluded = negative.iter().any(|n| glob::matches(n, &dir));
                if !excluded && self.root.join(&dir).join(marker_file).is_file() {
                    out.insert(dir);
                }
            }
        }
        out.into_iter().collect()
    }

    fn pnpm_and_package_json(&mut self) {
        if let Some(text) = read_layout_file(&self.root.join("pnpm-workspace.yaml")) {
            let patterns = sources::parse_pnpm_workspace(&text);
            for dir in self.expand_members(&patterns, "package.json") {
                self.add_member(&dir, ImportSource::PnpmWorkspace);
            }
        }
        if let Some(text) = read_layout_file(&self.root.join("package.json")) {
            let patterns = sources::parse_package_workspaces(&text);
            for dir in self.expand_members(&patterns, "package.json") {
                self.add_member(&dir, ImportSource::PackageJson);
            }
        }
    }

    fn cargo(&mut self) {
        let Some(text) = read_layout_file(&self.root.join("Cargo.toml")) else {
            return;
        };
        let ws = sources::parse_cargo_workspace(&text);
        let mut patterns = ws.members;
        patterns.retain(|m| m != ".");
        patterns.extend(ws.exclude.iter().map(|e| format!("!{e}")));
        for dir in self.expand_members(&patterns, "Cargo.toml") {
            self.add_member(&dir, ImportSource::CargoWorkspace);
        }
    }

    fn scan_folders(&mut self) {
        let Ok(read) = fs::read_dir(self.root) else {
            return;
        };
        let mut dirs: Vec<String> = read
            .flatten()
            .filter(|e| e.file_type().is_ok_and(|t| t.is_dir()))
            .filter_map(|e| e.file_name().into_string().ok())
            .filter(|n| !n.starts_with('.') && n != "node_modules" && n != "target")
            .collect();
        dirs.sort();
        for dir in dirs {
            let git = self.root.join(&dir).join(".git");
            if !git.exists() {
                continue;
            }
            if git.is_file()
                && read_small(&git)
                    .is_some_and(|t| t.contains("/worktrees/") || t.contains("\\worktrees\\"))
            {
                if !self.is_worktree(&dir) {
                    self.worktrees.push(PlannedWorktree {
                        path: dir,
                        kind: WorktreeKind::LinkedWorktree,
                    });
                }
                continue;
            }
            let track = self.current_branch_track(&dir);
            self.add(Candidate {
                original_name: dir.clone(),
                path: dir,
                root: None,
                remote: None,
                track,
                source: ImportSource::FolderScan,
            });
        }
    }

    fn finish(mut self, workspace_name: Name) -> ImportPlan {
        let mut taken: HashSet<String> = HashSet::new();
        let mut projects = Vec::new();
        let mut renames = Vec::new();
        for c in self.candidates {
            let slug = names::slugify(&c.original_name);
            let unique = names::unique(&slug, &taken);
            taken.insert(unique.clone());
            let Ok(name) = Name::new(unique.clone()) else {
                self.warnings.push(format!(
                    "could not derive a valid name for `{}`; skipped",
                    c.path
                ));
                continue;
            };
            let root = c.root.as_deref().and_then(|r| RepoPath::new(r).ok());
            let location = match &root {
                Some(r) => format!("{}#{}", c.path, r),
                None => c.path.clone(),
            };
            if unique != slug {
                renames.push(Rename {
                    location: location.clone(),
                    original: c.original_name.clone(),
                    assigned: name.clone(),
                    reason: RenameReason::Collision,
                });
            } else if slug != c.original_name {
                renames.push(Rename {
                    location,
                    original: c.original_name.clone(),
                    assigned: name.clone(),
                    reason: RenameReason::Slugified,
                });
            }
            projects.push(PlannedProject {
                name,
                path: c.path,
                root,
                remote: c.remote,
                track: c.track,
                source: c.source,
            });
        }
        self.worktrees.sort_by(|a, b| a.path.cmp(&b.path));
        ImportPlan {
            workspace_name,
            projects,
            worktrees: self.worktrees,
            renames,
            warnings: self.warnings,
        }
    }
}

#[cfg(test)]
mod tests;
