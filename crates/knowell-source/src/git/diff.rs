//! Commit diffs whose rename tracker sees only policy-permitted metadata.

use gix::error::ErrorExt;
use gix::hash::ObjectId;
use knowell_core::RepoPath;
use knowell_secrets::ExclusionPolicy;

use super::{Change, EntryKind, GitError, TreeEntry, failed, sort_changes, tree};
use crate::WalkOptions;

fn project_path(path: &RepoPath, root: Option<&RepoPath>) -> Option<RepoPath> {
    match root {
        None => Some(path.clone()),
        Some(root) => path
            .as_str()
            .strip_prefix(root.as_str())
            .and_then(|suffix| suffix.strip_prefix('/'))
            .and_then(|suffix| RepoPath::new(suffix).ok()),
    }
}

fn filtered_tree<'repo>(
    repo: &'repo gix::Repository,
    entries: &[TreeEntry],
    root: Option<&RepoPath>,
    policy: &ExclusionPolicy,
) -> Result<gix::Tree<'repo>, GitError> {
    let builtin = ExclusionPolicy::builtin();
    let mut editor = repo
        .empty_tree()
        .edit()
        .map_err(failed("filtered tree setup"))?;
    for entry in entries {
        let mode = match entry.mode {
            EntryKind::File => gix::objs::tree::EntryKind::Blob,
            EntryKind::Executable => gix::objs::tree::EntryKind::BlobExecutable,
            EntryKind::Symlink | EntryKind::Submodule => continue,
        };
        let Some(relative) = project_path(&entry.path, root) else {
            continue;
        };
        // Stripping a sensitive parent root must not remove its built-in exclusion.
        if builtin.check(&entry.path).is_some() || policy.check(&relative).is_some() {
            continue;
        }
        let id = ObjectId::from_hex(entry.blob.as_bytes())
            .map_err(|error| failed("tree object id")(error.into_error()))?;
        editor
            .upsert(entry.path.as_str(), mode, id)
            .map_err(failed("filtered tree entry"))?;
    }
    let id = editor.write().map_err(failed("filtered tree creation"))?;
    repo.find_tree(id).map_err(failed("filtered tree lookup"))
}

pub(super) fn scoped(
    repo: &gix::Repository,
    old: &[TreeEntry],
    new: &[TreeEntry],
    root: Option<&RepoPath>,
    policy: &ExclusionPolicy,
    options: &WalkOptions,
) -> Result<Vec<Change>, GitError> {
    let old = filtered_tree(repo, old, root, policy)?;
    let new = filtered_tree(repo, new, root, policy)?;
    // Empty IdMapping attributes have no global files, info file or per-path IDs.
    // The default plumbing filter has no configured drivers or worktree roots.
    let attributes = gix::worktree::Stack::new(
        repo.git_dir().to_path_buf(),
        gix::worktree::stack::State::AttributesStack(Default::default()),
        gix::glob::pattern::Case::Sensitive,
        Vec::new(),
        Vec::new(),
    );
    let pipeline = gix::diff::blob::Pipeline::new(
        Default::default(),
        gix::filter::plumbing::Pipeline::default(),
        Vec::new(),
        gix::diff::blob::pipeline::Options {
            large_file_threshold_bytes: options.max_file_bytes,
            ..Default::default()
        },
    );
    let mut cache = gix::diff::blob::Platform::new(
        gix::diff::blob::platform::Options {
            algorithm: Some(gix::diff::blob::Algorithm::Histogram),
            skip_internal_diff_if_external_is_configured: false,
        },
        pipeline,
        gix::diff::blob::pipeline::Mode::ToGit,
        attributes,
    );
    let mut rewrites = gix::diff::Rewrites::default();
    // In gix, a zero binary threshold means unlimited; a zero read limit here
    // instead permits metadata-only matching by exact object identity.
    if options.max_file_bytes == 0 {
        rewrites.percentage = None;
    }
    let mut changes = Vec::new();
    gix::diff::tree_with_rewrites(
        gix::objs::TreeRefIter::from_bytes(&old.data, repo.object_hash()),
        gix::objs::TreeRefIter::from_bytes(&new.data, repo.object_hash()),
        &mut cache,
        &mut gix::diff::tree::State::default(),
        &repo.objects,
        |change| {
            tree::convert_change(change.into_owned(), &mut changes);
            Ok(std::ops::ControlFlow::Continue(()))
        },
        gix::diff::tree_with_rewrites::Options {
            location: Some(gix::diff::tree::recorder::Location::Path),
            rewrites: Some(rewrites),
        },
    )
    .map_err(|error| failed("scoped tree diff")(error.raise().into_error()))?;
    sort_changes(&mut changes);
    Ok(changes)
}
