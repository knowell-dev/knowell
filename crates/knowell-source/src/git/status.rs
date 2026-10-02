//! A worktree's uncommitted changes relative to its own `HEAD`.
//!
//! git keeps two comparisons: `HEAD` → index (staged) and index → worktree
//! (unstaged, plus untracked files). The overlay needs their composition,
//! `HEAD` → worktree, which is derived per path here. Using the index keeps
//! the stat cache: unchanged files are not re-hashed.

use std::collections::BTreeMap;

use gix::bstr::BString;
use gix::dir::entry::{Kind as DiskKind, Status as DirStatus};
use gix::status::index_worktree::{Item as WorktreeItem, RewriteSource};
use gix::status::plumbing::index_as_worktree::{Change as WorktreeChange, EntryStatus};
use knowell_secrets::ExclusionPolicy;

use super::tree::repo_path;
use super::{Change, GitError, failed};

/// `HEAD` → index state of one path.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Staged {
    Added,
    Deleted,
    Modified,
}

/// Index → worktree state of one path.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Unstaged {
    /// Tracked and different on disk (content, mode or type).
    Modified,
    /// Tracked and missing on disk.
    Removed,
    /// On disk, not tracked and not ignored (or intent-to-add).
    Untracked,
}

#[derive(Debug, Default)]
struct PathState {
    staged: Option<Staged>,
    unstaged: Option<Unstaged>,
}

/// Composes the two comparisons into the `HEAD` → worktree change of one
/// path, `None` when the two cancel out (staged addition deleted again).
fn compose(state: &PathState) -> Option<ChangeKind> {
    match (state.staged, state.unstaged) {
        (Some(Staged::Added), Some(Unstaged::Removed)) => None,
        (Some(Staged::Added), _) => Some(ChangeKind::Added),
        // `git rm --cached`: gone from the index, still on disk.
        (Some(Staged::Deleted), Some(Unstaged::Untracked)) => Some(ChangeKind::Modified),
        (Some(Staged::Deleted), _) => Some(ChangeKind::Deleted),
        (Some(Staged::Modified), Some(Unstaged::Removed)) => Some(ChangeKind::Deleted),
        (Some(Staged::Modified), _) => Some(ChangeKind::Modified),
        (None, Some(Unstaged::Modified)) => Some(ChangeKind::Modified),
        (None, Some(Unstaged::Removed)) => Some(ChangeKind::Deleted),
        (None, Some(Unstaged::Untracked)) => Some(ChangeKind::Added),
        (None, None) => None,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ChangeKind {
    Added,
    Modified,
    Deleted,
}

fn is_submodule(mode: gix::index::entry::Mode) -> bool {
    mode == gix::index::entry::Mode::COMMIT
}

fn record_tree_index(change: &gix::diff::index::Change, states: &mut BTreeMap<BString, PathState>) {
    use gix::diff::index::ChangeRef;
    let mut set = |path: &gix::bstr::BStr, staged: Staged| {
        states.entry(path.to_owned()).or_default().staged = Some(staged);
    };
    match change {
        ChangeRef::Addition {
            location,
            entry_mode,
            ..
        } if !is_submodule(*entry_mode) => set(location.as_ref(), Staged::Added),
        ChangeRef::Deletion {
            location,
            entry_mode,
            ..
        } if !is_submodule(*entry_mode) => set(location.as_ref(), Staged::Deleted),
        ChangeRef::Modification {
            location,
            previous_entry_mode,
            entry_mode,
            ..
        } => match (
            is_submodule(*previous_entry_mode),
            is_submodule(*entry_mode),
        ) {
            (false, false) => set(location.as_ref(), Staged::Modified),
            (false, true) => set(location.as_ref(), Staged::Deleted),
            (true, false) => set(location.as_ref(), Staged::Added),
            (true, true) => {}
        },
        // Rename tracking is disabled; should one arrive anyway, keep both
        // sides as a deletion plus an addition.
        ChangeRef::Rewrite {
            source_location,
            source_entry_mode,
            location,
            entry_mode,
            copy,
            ..
        } => {
            if !copy && !is_submodule(*source_entry_mode) {
                set(source_location.as_ref(), Staged::Deleted);
            }
            if !is_submodule(*entry_mode) {
                set(location.as_ref(), Staged::Added);
            }
        }
        _ => {}
    }
}

fn record_index_worktree(item: &WorktreeItem, states: &mut BTreeMap<BString, PathState>) {
    let mut set = |path: &gix::bstr::BStr, unstaged: Unstaged| {
        states.entry(path.to_owned()).or_default().unstaged = Some(unstaged);
    };
    let untracked_file = |entry: &gix::dir::Entry| {
        entry.status == DirStatus::Untracked
            && matches!(entry.disk_kind, Some(DiskKind::File | DiskKind::Symlink))
    };
    match item {
        WorktreeItem::Modification {
            entry,
            rela_path,
            status,
            ..
        } => {
            if is_submodule(entry.mode) {
                return;
            }
            let state = match status {
                EntryStatus::Change(WorktreeChange::Removed) => Unstaged::Removed,
                EntryStatus::Change(
                    WorktreeChange::Modification { .. } | WorktreeChange::Type { .. },
                )
                | EntryStatus::Conflict { .. } => Unstaged::Modified,
                EntryStatus::IntentToAdd => Unstaged::Untracked,
                EntryStatus::Change(WorktreeChange::SubmoduleModification(_))
                | EntryStatus::NeedsUpdate(_) => return,
            };
            set(rela_path.as_ref(), state);
        }
        WorktreeItem::DirectoryContents { entry, .. } => {
            if untracked_file(entry) {
                set(entry.rela_path.as_ref(), Unstaged::Untracked);
            }
        }
        // Worktree rename tracking is disabled; handle it as removal plus
        // untracked addition should it appear.
        WorktreeItem::Rewrite {
            source,
            dirwalk_entry,
            copy,
            ..
        } => {
            if let RewriteSource::RewriteFromIndex {
                source_rela_path, ..
            } = source
                && !copy
            {
                set(source_rela_path.as_ref(), Unstaged::Removed);
            }
            if untracked_file(dirwalk_entry) {
                set(dirwalk_entry.rela_path.as_ref(), Unstaged::Untracked);
            }
        }
    }
}

/// Computes `HEAD` → worktree changes for an opened (non-bare) worktree.
/// The result is unsorted; the caller sorts.
pub(super) fn working_changes(
    repo: &gix::Repository,
    policy: &ExclusionPolicy,
) -> Result<Vec<Change>, GitError> {
    let iter = repo
        .status(gix::progress::Discard)
        .map_err(failed("status"))?
        .untracked_files(gix::status::UntrackedFiles::Files)
        .index_worktree_submodules(None)
        .index_worktree_rewrites(None)
        .tree_index_track_renames(gix::status::tree_index::TrackRenames::Disabled)
        .into_iter(Vec::<BString>::new())
        .map_err(failed("status"))?;

    let mut states: BTreeMap<BString, PathState> = BTreeMap::new();
    for item in iter {
        match item.map_err(failed("status"))? {
            gix::status::Item::TreeIndex(change) => record_tree_index(&change, &mut states),
            gix::status::Item::IndexWorktree(item) => record_index_worktree(&item, &mut states),
        }
    }

    let mut changes = Vec::with_capacity(states.len());
    for (raw, state) in &states {
        let Some(kind) = compose(state) else {
            continue;
        };
        let Some(path) = repo_path(raw.as_ref()) else {
            continue;
        };
        if policy.check(&path).is_some() {
            continue;
        }
        changes.push(match kind {
            ChangeKind::Added => Change::Added(path),
            ChangeKind::Modified => Change::Modified(path),
            ChangeKind::Deleted => Change::Deleted(path),
        });
    }
    Ok(changes)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn state(staged: Option<Staged>, unstaged: Option<Unstaged>) -> PathState {
        PathState { staged, unstaged }
    }

    #[test]
    fn composition_table() {
        use ChangeKind as K;
        use Staged as S;
        use Unstaged as U;
        let cases = [
            (None, None, None),
            (None, Some(U::Modified), Some(K::Modified)),
            (None, Some(U::Removed), Some(K::Deleted)),
            (None, Some(U::Untracked), Some(K::Added)),
            (Some(S::Added), None, Some(K::Added)),
            (Some(S::Added), Some(U::Modified), Some(K::Added)),
            (Some(S::Added), Some(U::Removed), None),
            (Some(S::Deleted), None, Some(K::Deleted)),
            (Some(S::Deleted), Some(U::Untracked), Some(K::Modified)),
            (Some(S::Modified), None, Some(K::Modified)),
            (Some(S::Modified), Some(U::Modified), Some(K::Modified)),
            (Some(S::Modified), Some(U::Removed), Some(K::Deleted)),
        ];
        for (staged, unstaged, expected) in cases {
            assert_eq!(
                compose(&state(staged, unstaged)),
                expected,
                "{staged:?} {unstaged:?}"
            );
        }
    }
}
