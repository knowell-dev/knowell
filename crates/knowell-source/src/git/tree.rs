//! Tree traversal and file reads from git objects.

use gix::bstr::{BStr, BString, ByteSlice};
use gix::hash::ObjectId;
use gix::objs::tree::{EntryKind as GitKind, EntryMode};
use knowell_core::RepoPath;
use knowell_secrets::{Exclusion, ExclusionPolicy};

use super::{Change, GitError, describe, failed};
use crate::content::{self, Outcome};
use crate::fs::{FileRead, SkipReason, SkippedFile, WalkOptions, WalkReport};

/// A node met during [`visit`].
pub(super) enum Node {
    /// A subtree; the visitor returns whether to descend.
    Dir,
    /// Any other entry.
    Entry(GitKind, ObjectId),
}

/// Visits every entry below `root` depth-first, giving each its full
/// `/`-joined path. Iterative, so hostile nesting cannot overflow the stack.
/// The visitor's return value only matters for [`Node::Dir`].
pub(super) fn visit(
    repo: &gix::Repository,
    root: ObjectId,
    mut visitor: impl FnMut(&BStr, Node) -> bool,
) -> Result<(), GitError> {
    let mut stack: Vec<(BString, ObjectId)> = vec![(BString::default(), root)];
    while let Some((prefix, id)) = stack.pop() {
        let tree = repo.find_tree(id).map_err(failed("tree lookup"))?;
        let decoded = tree.decode().map_err(|e| GitError::Git {
            operation: "tree decoding",
            message: describe(&e.into_error()),
        })?;
        for entry in decoded.entries {
            let mut path = prefix.clone();
            if !path.is_empty() {
                path.push(b'/');
            }
            path.extend_from_slice(entry.filename);
            let kind = entry.mode.kind();
            if kind == GitKind::Tree {
                if visitor(path.as_ref(), Node::Dir) {
                    stack.push((path, entry.oid.to_owned()));
                }
            } else {
                visitor(path.as_ref(), Node::Entry(kind, entry.oid.to_owned()));
            }
        }
    }
    Ok(())
}

/// `raw` as a [`RepoPath`], if it is valid UTF-8 and a valid path.
pub(super) fn repo_path(raw: &BStr) -> Option<RepoPath> {
    RepoPath::new(raw.to_str().ok()?).ok()
}

/// A [`RepoPath`] for reporting a name that is not a valid one: components
/// are rendered lossily and characters a [`RepoPath`] cannot hold (`\`,
/// NUL, and `:` which could form a drive prefix) become `?`.
fn lossy_path(raw: &BStr) -> Option<RepoPath> {
    let parts: Vec<String> = raw
        .split_str("/")
        .map(|part| {
            let text = part.to_str_lossy().replace(['\\', '\0', ':'], "?");
            if text.is_empty() || text == "." || text == ".." {
                "?".to_owned()
            } else {
                text
            }
        })
        .collect();
    RepoPath::new(parts.join("/")).ok()
}

/// Reads one blob: size from the header before reading, then the shared
/// content pipeline. Object-level problems become skip reasons so that one
/// bad or missing blob does not abort a whole walk.
fn read_blob(
    repo: &gix::Repository,
    path: RepoPath,
    id: ObjectId,
    options: &WalkOptions,
) -> Outcome {
    let unreadable = |kind: &str| {
        Outcome::Skip(SkipReason::Unreadable {
            error_kind: kind.to_owned(),
        })
    };
    let header = match repo.try_find_header(id) {
        Ok(Some(header)) => header,
        Ok(None) => return unreadable("NotFound"),
        Err(_) => return unreadable("Other"),
    };
    if header.kind() != gix::objs::Kind::Blob {
        return unreadable("InvalidData");
    }
    let size = header.size();
    if size > options.max_file_bytes {
        return Outcome::Skip(SkipReason::TooLarge { size });
    }
    let Ok(blob) = repo.find_blob(id) else {
        return unreadable("Other");
    };
    let size = blob.data.len() as u64;
    if size > options.max_file_bytes {
        return Outcome::Skip(SkipReason::TooLarge { size });
    }
    content::decode(path, &blob.data)
}

#[derive(Default)]
struct Report {
    files: Vec<crate::fs::SourceFile>,
    skipped: Vec<SkippedFile>,
}

impl Report {
    fn skip(&mut self, path: RepoPath, reason: SkipReason) {
        self.skipped.push(SkippedFile { path, reason });
    }

    fn excluded(&mut self, path: RepoPath, exclusion: Exclusion) {
        // `.git` / `.knowell` internals are never reported.
        if exclusion != Exclusion::Internal {
            self.skip(path, SkipReason::Excluded(exclusion));
        }
    }

    /// Handles one non-directory entry whose path passed exclusion.
    fn entry(
        &mut self,
        repo: &gix::Repository,
        path: RepoPath,
        kind: GitKind,
        id: ObjectId,
        options: &WalkOptions,
    ) {
        match kind {
            GitKind::Blob | GitKind::BlobExecutable => {
                match read_blob(repo, path.clone(), id, options) {
                    Outcome::File(file) => self.files.push(*file),
                    Outcome::Skip(reason) => self.skip(path, reason),
                }
            }
            GitKind::Link => self.skip(path, SkipReason::Symlink),
            // Submodules have no content here; trees are handled by callers.
            GitKind::Commit | GitKind::Tree => {}
        }
    }

    fn finish(mut self) -> WalkReport {
        self.files.sort_by(|a, b| a.path.cmp(&b.path));
        self.skipped.sort_by(|a, b| a.path.cmp(&b.path));
        WalkReport {
            files: self.files,
            skipped: self.skipped,
        }
    }
}

/// Reads every file below `root` (see [`super::GitRepo::walk_commit`]).
pub(super) fn walk(
    repo: &gix::Repository,
    root: ObjectId,
    policy: &ExclusionPolicy,
    options: &WalkOptions,
) -> Result<WalkReport, GitError> {
    let mut report = Report::default();
    let mut failure = None;
    visit(repo, root, |raw, node| match node {
        Node::Dir => {
            // An invalid directory name cannot be checked as a directory;
            // its files are reported one by one as invalid paths instead.
            let Some(dir) = repo_path(raw) else {
                return true;
            };
            match policy.check_dir(&dir) {
                None => true,
                Some(exclusion) => {
                    report.excluded(dir, exclusion);
                    false
                }
            }
        }
        Node::Entry(kind, id) => {
            let Some(path) = repo_path(raw) else {
                match lossy_path(raw) {
                    Some(lossy) => report.skip(lossy, SkipReason::InvalidPath),
                    None => {
                        failure.get_or_insert(GitError::Git {
                            operation: "tree walk",
                            message: "an entry name cannot be reported".to_owned(),
                        });
                    }
                }
                return false;
            };
            match policy.check(&path) {
                Some(exclusion) => report.excluded(path, exclusion),
                None => report.entry(repo, path, kind, id, options),
            }
            false
        }
    })?;
    if let Some(error) = failure {
        return Err(error);
    }
    Ok(report.finish())
}

/// Reads selected paths of `root` (see [`super::GitRepo::read_commit_files`]).
pub(super) fn read_paths(
    repo: &gix::Repository,
    commit: &str,
    root: ObjectId,
    paths: &[RepoPath],
    policy: &ExclusionPolicy,
    options: &WalkOptions,
) -> Result<WalkReport, GitError> {
    let tree = repo.find_tree(root).map_err(failed("tree lookup"))?;
    let mut unique: Vec<&RepoPath> = paths.iter().collect();
    unique.sort();
    unique.dedup();
    let mut report = Report::default();
    for path in unique {
        if let Some(exclusion) = policy.check(path) {
            report.excluded(path.clone(), exclusion);
            continue;
        }
        let not_found = || GitError::PathNotFound {
            commit: commit.to_owned(),
            path: path.clone(),
        };
        let entry = tree
            .lookup_entry(path.components())
            .map_err(failed("tree lookup"))?
            .ok_or_else(not_found)?;
        let kind = entry.mode().kind();
        if kind == GitKind::Tree {
            return Err(not_found());
        }
        report.entry(repo, path.clone(), kind, entry.object_id(), options);
    }
    Ok(report.finish())
}

/// Reads one path of `root` (see [`super::GitRepo::read_commit_file`]).
pub(super) fn read_one(
    repo: &gix::Repository,
    root: ObjectId,
    path: &RepoPath,
    policy: &ExclusionPolicy,
    options: &WalkOptions,
) -> Result<FileRead, GitError> {
    if let Some(exclusion) = policy.check(path) {
        return Ok(FileRead::Skipped(SkipReason::Excluded(exclusion)));
    }
    let tree = repo.find_tree(root).map_err(failed("tree lookup"))?;
    let Some(entry) = tree
        .lookup_entry(path.components())
        .map_err(failed("tree lookup"))?
    else {
        return Ok(FileRead::Missing);
    };
    Ok(match entry.mode().kind() {
        GitKind::Blob | GitKind::BlobExecutable => {
            match read_blob(repo, path.clone(), entry.object_id(), options) {
                Outcome::File(file) => FileRead::File(*file),
                Outcome::Skip(reason) => FileRead::Skipped(reason),
            }
        }
        GitKind::Link => FileRead::Skipped(SkipReason::Symlink),
        // A directory, or a submodule (no content in this repository).
        GitKind::Tree | GitKind::Commit => FileRead::Missing,
    })
}

/// Whether a tree entry with `mode` is file content in this repository
/// (not a directory, not a submodule).
fn file_like(mode: EntryMode) -> bool {
    !mode.is_tree() && !mode.is_commit()
}

/// Similarity in percent, from gix's `0.0..=1.0` ratio.
fn percent(ratio: f32) -> u8 {
    // `as` saturates and maps NaN to 0, so hostile input cannot overflow.
    (ratio * 100.0).round().clamp(0.0, 100.0) as u8
}

/// Converts one gix tree change into file-level [`Change`]s.
pub(super) fn convert_change(
    change: gix::object::tree::diff::ChangeDetached,
    out: &mut Vec<Change>,
) {
    use gix::diff::tree_with_rewrites::Change as Raw;
    let path = |raw: &BString| repo_path(raw.as_ref());
    match change {
        Raw::Addition {
            location,
            entry_mode,
            ..
        } => {
            if file_like(entry_mode) {
                out.extend(path(&location).map(Change::Added));
            }
        }
        Raw::Deletion {
            location,
            entry_mode,
            ..
        } => {
            if file_like(entry_mode) {
                out.extend(path(&location).map(Change::Deleted));
            }
        }
        Raw::Modification {
            location,
            previous_entry_mode,
            entry_mode,
            ..
        } => {
            let Some(p) = path(&location) else {
                return;
            };
            match (file_like(previous_entry_mode), file_like(entry_mode)) {
                (true, true) => out.push(Change::Modified(p)),
                (true, false) => out.push(Change::Deleted(p)),
                (false, true) => out.push(Change::Added(p)),
                (false, false) => {}
            }
        }
        Raw::Rewrite {
            source_location,
            source_entry_mode,
            diff,
            entry_mode,
            location,
            copy,
            ..
        } => {
            // A copy leaves its source in place: only the target is new.
            let from = (file_like(source_entry_mode) && !copy)
                .then(|| path(&source_location))
                .flatten();
            let to = file_like(entry_mode).then(|| path(&location)).flatten();
            match (from, to) {
                (Some(from), Some(to)) => out.push(Change::Renamed {
                    from,
                    to,
                    similarity: diff.map_or(100, |d| percent(d.similarity)),
                }),
                (Some(from), None) => out.push(Change::Deleted(from)),
                (None, Some(to)) => out.push(Change::Added(to)),
                (None, None) => {}
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn b(s: &[u8]) -> &BStr {
        s.as_bstr()
    }

    #[test]
    fn repo_path_rejects_unrepresentable_names() {
        assert_eq!(repo_path(b(b"src/a.rs")).unwrap().as_str(), "src/a.rs");
        assert!(repo_path(b(b"bad\xffname")).is_none());
        assert!(repo_path(b(b"a\\b")).is_none());
        assert!(repo_path(b(b"c:x")).is_none());
        assert!(repo_path(b(b"a/../b")).is_none());
        assert!(repo_path(b(b"")).is_none());
    }

    #[test]
    fn lossy_paths_are_always_valid() {
        for raw in [
            &b"bad\xffname"[..],
            b"a\\b/c",
            b"c:x",
            b"a/../b",
            b"",
            b"/",
            b"x\0y",
            b"./.",
        ] {
            let p = lossy_path(b(raw)).unwrap();
            assert!(!p.as_str().contains('\\'), "{raw:?}");
        }
        assert_eq!(lossy_path(b(b"c:x/y")).unwrap().as_str(), "c?x/y");
    }

    #[test]
    fn percent_is_clamped() {
        assert_eq!(percent(1.0), 100);
        assert_eq!(percent(0.5), 50);
        assert_eq!(percent(0.666), 67);
        assert_eq!(percent(f32::NAN), 0);
        assert_eq!(percent(7.0), 100);
        assert_eq!(percent(-1.0), 0);
    }
}
