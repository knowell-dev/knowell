//! A worktree's uncommitted changes relative to its own `HEAD`.
//!
//! git keeps two comparisons: `HEAD` → index (staged) and index → worktree
//! (unstaged, plus untracked files). The overlay needs their composition,
//! `HEAD` → worktree, which is derived per path here. Using the index keeps
//! the stat cache: unchanged files are not re-hashed.

use std::collections::{BTreeMap, BTreeSet};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::AtomicBool;

use gix::bstr::{BStr, BString, ByteSlice};
use gix::error::ErrorExt;
use gix::objs::{FindExt, FindHeader};
use gix::status::plumbing::index_as_worktree::{Change as WorktreeChange, EntryStatus};
use knowell_core::RepoPath;
use knowell_secrets::ExclusionPolicy;

use super::tree::repo_path;
use super::{Change, GitError, failed};
use crate::WalkOptions;

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

fn record_tree_index(
    change: &gix::diff::index::ChangeRef<'_, '_>,
    states: &mut BTreeMap<BString, PathState>,
) {
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

fn record_index_worktree(
    path: &BStr,
    mode: gix::index::entry::Mode,
    status: &EntryStatus,
    states: &mut BTreeMap<BString, PathState>,
) {
    if is_submodule(mode) {
        return;
    }
    let state = match status {
        EntryStatus::Change(WorktreeChange::Removed) => Unstaged::Removed,
        EntryStatus::Change(WorktreeChange::Modification { .. } | WorktreeChange::Type { .. })
        | EntryStatus::Conflict { .. } => Unstaged::Modified,
        EntryStatus::IntentToAdd => Unstaged::Untracked,
        EntryStatus::Change(WorktreeChange::SubmoduleModification(_))
        | EntryStatus::NeedsUpdate(_) => return,
    };
    states.entry(path.to_owned()).or_default().unstaged = Some(state);
}

fn incomplete(message: &'static str) -> GitError {
    GitError::Git {
        operation: "scoped status",
        message: message.to_owned(),
    }
}

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

fn permitted(path: &RepoPath, root: Option<&RepoPath>, policy: &ExclusionPolicy) -> bool {
    ExclusionPolicy::builtin().check(path).is_none()
        && project_path(path, root).is_some_and(|relative| policy.check(&relative).is_none())
}

fn metadata(workdir: &Path, path: &RepoPath) -> Result<Option<std::fs::Metadata>, GitError> {
    let mut full = workdir.to_path_buf();
    let mut last = None;
    for component in path.components() {
        full.push(component);
        match std::fs::symlink_metadata(&full) {
            Ok(value) if value.file_type().is_symlink() => {
                return Err(incomplete(
                    "working tree status is incomplete: symbolic links are not supported",
                ));
            }
            Ok(value) => last = Some(value),
            Err(error)
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::NotFound | std::io::ErrorKind::NotADirectory
                ) =>
            {
                return Ok(None);
            }
            Err(_) => {
                return Err(incomplete(
                    "working tree status is incomplete: file metadata could not be read",
                ));
            }
        }
    }
    Ok(last)
}

fn check_file(value: &std::fs::Metadata, options: &WalkOptions) -> Result<(), GitError> {
    if !value.is_file() {
        return Err(incomplete(
            "working tree status is incomplete: a tracked path is not a regular file",
        ));
    }
    if value.len() > options.max_file_bytes {
        return Err(incomplete(
            "working tree status is incomplete: a permitted file exceeds max_file_bytes",
        ));
    }
    Ok(())
}

// Capturing controls never adds a source candidate: the original content
// scope remains independent, including any already-permitted control files.
const CONTROL_FILE_LIMIT: u64 = 64 * 1024;
const CONTROL_TOTAL_LIMIT: usize = 1024 * 1024;
const CONTROL_COUNT_LIMIT: usize = 1024;

#[derive(Default)]
struct Controls {
    files: BTreeMap<PathBuf, Option<Vec<u8>>>,
    bytes: usize,
    present: usize,
}

impl Controls {
    fn remember(
        &mut self,
        path: &Path,
        bytes: Option<Vec<u8>>,
    ) -> Result<Option<Vec<u8>>, GitError> {
        self.bytes = self
            .bytes
            .checked_add(bytes.as_ref().map_or(0, Vec::len))
            .ok_or_else(|| {
                incomplete(
                    "working tree status is incomplete: Git control metadata exceeds its limit",
                )
            })?;
        self.present = self
            .present
            .checked_add(usize::from(bytes.is_some()))
            .ok_or_else(|| {
                incomplete(
                    "working tree status is incomplete: Git control metadata exceeds its limit",
                )
            })?;
        if self.bytes > CONTROL_TOTAL_LIMIT || self.present > CONTROL_COUNT_LIMIT {
            return Err(incomplete(
                "working tree status is incomplete: Git control metadata exceeds its limit",
            ));
        }
        self.files.insert(path.to_path_buf(), bytes.clone());
        Ok(bytes)
    }

    fn read(&mut self, boundary: &Path, path: &Path) -> Result<Option<Vec<u8>>, GitError> {
        if let Some(bytes) = self.files.get(path) {
            return Ok(bytes.clone());
        }
        let relative = path.strip_prefix(boundary).map_err(|_| {
            incomplete(
                "working tree status is incomplete: Git control metadata is outside the repository",
            )
        })?;
        let mut current = boundary.to_path_buf();
        for component in std::iter::once(None).chain(relative.components().map(Some)) {
            if let Some(component) = component {
                if !matches!(component, std::path::Component::Normal(_)) {
                    return Err(incomplete(
                        "working tree status is incomplete: Git control metadata path is invalid",
                    ));
                }
                current.push(component);
            }
            match std::fs::symlink_metadata(&current) {
                Ok(value) if value.file_type().is_symlink() => {
                    return Err(incomplete(
                        "working tree status is incomplete: Git control metadata is a symbolic link",
                    ));
                }
                Ok(value) if current == path => {
                    if !value.is_file() || value.len() > CONTROL_FILE_LIMIT {
                        return Err(incomplete(
                            "working tree status is incomplete: Git control metadata is not a bounded regular file",
                        ));
                    }
                }
                Ok(value) if value.is_dir() => {}
                Ok(_) => {
                    return Err(incomplete(
                        "working tree status is incomplete: Git control metadata ancestor is not a directory",
                    ));
                }
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                    return self.remember(path, None);
                }
                Err(_) => {
                    return Err(incomplete(
                        "working tree status is incomplete: Git control metadata could not be checked",
                    ));
                }
            }
        }
        let file = std::fs::File::open(path).map_err(|_| {
            incomplete("working tree status is incomplete: Git control metadata could not be read")
        })?;
        let mut bytes = Vec::new();
        file.take(CONTROL_FILE_LIMIT + 1)
            .read_to_end(&mut bytes)
            .map_err(|_| {
                incomplete(
                    "working tree status is incomplete: Git control metadata could not be read",
                )
            })?;
        if bytes.len() as u64 > CONTROL_FILE_LIMIT {
            return Err(incomplete(
                "working tree status is incomplete: Git control metadata exceeds its file limit",
            ));
        }
        if std::str::from_utf8(&bytes).is_err() || bytes.contains(&0) {
            return Err(incomplete(
                "working tree status is incomplete: Git control metadata is not valid text",
            ));
        }
        self.remember(path, Some(bytes))
    }
}

fn reject_nonempty_control(path: &Path, message: &'static str) -> Result<(), GitError> {
    match std::fs::symlink_metadata(path) {
        Ok(value) if value.is_file() && value.len() == 0 => Ok(()),
        Ok(_) => Err(incomplete(message)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(_) => Err(incomplete(
            "working tree status is incomplete: ignore control metadata could not be checked",
        )),
    }
}

fn ignore_rules(
    directory: &Path,
    source: &Path,
    bytes: &[u8],
    ignore_case: bool,
) -> Result<ignore::gitignore::Gitignore, GitError> {
    let text = std::str::from_utf8(bytes).map_err(|_| {
        incomplete("working tree status is incomplete: an approved ignore file is not utf-8")
    })?;
    let mut builder = ignore::gitignore::GitignoreBuilder::new(directory);
    builder.case_insensitive(ignore_case).map_err(|_| {
        incomplete("working tree status is incomplete: ignore case rules could not be configured")
    })?;
    for line in text.lines() {
        builder
            .add_line(Some(source.to_path_buf()), line)
            .map_err(|_| {
                incomplete("working tree status is incomplete: an approved ignore rule is invalid")
            })?;
    }
    builder.build().map_err(|_| {
        incomplete("working tree status is incomplete: approved ignore rules could not be compiled")
    })
}

fn ignored(path: &Path, directory: bool, rules: &[ignore::gitignore::Gitignore]) -> bool {
    for rules in rules.iter().rev() {
        let matched = rules.matched(path, directory);
        if !matched.is_none() {
            return matched.is_ignore();
        }
    }
    false
}

fn configured_control(
    repo: &gix::Repository,
    key: &str,
    recognized: &[PathBuf],
    controls: &mut Controls,
) -> Result<Option<(PathBuf, Vec<u8>)>, GitError> {
    let config = repo.config_snapshot();
    let Some(value) = config.string(key) else {
        return Ok(None);
    };
    let path = gix::path::from_bstr(value.as_bstr()).into_owned();
    if !path.is_absolute() || !recognized.contains(&path) {
        // Missing or empty regular external controls have no rule content.
        // This check intentionally does not open them.
        if !path.is_absolute() {
            return Err(incomplete(
                "working tree status is incomplete: configured external Git controls are not supported",
            ));
        }
        reject_nonempty_control(
            &path,
            "working tree status is incomplete: configured external Git controls are not supported",
        )?;
        return Ok(None);
    }
    let boundary = if path.starts_with(repo.common_dir()) {
        repo.common_dir()
    } else {
        repo.workdir()
            .ok_or_else(|| incomplete("working tree status requires a worktree"))?
    };
    Ok(controls.read(boundary, &path)?.map(|bytes| (path, bytes)))
}

#[derive(Default)]
struct InheritedIgnores {
    rules: Vec<ignore::gitignore::Gitignore>,
    root_ignored: bool,
}

fn inherited_ignores(
    repo: &gix::Repository,
    workdir: &Path,
    root: Option<&RepoPath>,
    controls: &mut Controls,
    ignore_case: bool,
) -> Result<InheritedIgnores, GitError> {
    let mut rules = Vec::new();
    let info = repo.common_dir().join("info/exclude");
    if let Some((source, bytes)) = configured_control(
        repo,
        "core.excludesFile",
        &[workdir.join(".gitignore"), info.clone()],
        controls,
    )? {
        rules.push(ignore_rules(workdir, &source, &bytes, ignore_case)?);
    }
    if let Some(bytes) = controls.read(repo.common_dir(), &info)? {
        rules.push(ignore_rules(workdir, &info, &bytes, ignore_case)?);
    }
    let mut directory = workdir.to_path_buf();
    let mut root_ignored = false;
    if let Some(root) = root {
        for component in root.components() {
            if !root_ignored {
                let source = directory.join(".gitignore");
                if let Some(bytes) = controls.read(workdir, &source)? {
                    rules.push(ignore_rules(&directory, &source, &bytes, ignore_case)?);
                }
            }
            directory.push(component);
            // Git cannot re-include descendants without re-including their
            // parent first; a nested ignore file cannot resurrect this root.
            root_ignored |= ignored(&directory, true, &rules);
        }
    }
    Ok(InheritedIgnores {
        rules,
        root_ignored,
    })
}

fn bounded_blob(
    objects: &gix::OdbHandle,
    id: &gix::hash::ObjectId,
    limit: u64,
    buffer: &mut Vec<u8>,
) -> gix::ExnResult<Option<()>> {
    buffer.clear();
    if id.is_null() {
        return Ok(None);
    }
    if id.is_empty_blob() {
        return Ok(Some(()));
    }
    let header = objects.try_header(id).map_err(|_| {
        gix::error::message(
            "working tree status is incomplete: an approved index blob header could not be read",
        )
        .raise_erased()
    })?;
    let Some(header) = header else {
        return Err(gix::error::message(
            "working tree status is incomplete: an approved index blob is missing",
        )
        .raise_erased());
    };
    if header.kind != gix::objs::Kind::Blob || header.size > limit {
        return Err(gix::error::message(
            "working tree status is incomplete: an approved index blob exceeds its read limit",
        )
        .raise_erased());
    }
    let blob = objects.find_blob(id, buffer).map_err(|_| {
        gix::error::message(
            "working tree status is incomplete: an approved index blob could not be read",
        )
        .raise_erased()
    })?;
    if blob.data.len() as u64 > limit {
        return Err(gix::error::message(
            "working tree status is incomplete: an approved index blob exceeds its read limit",
        )
        .raise_erased());
    }
    Ok(Some(()))
}

#[derive(Clone)]
struct Normalization {
    attributes: Arc<gix::attrs::Search>,
    collection: Arc<gix::attrs::search::MetadataCollection>,
    case: gix::glob::pattern::Case,
    normalized: Arc<BTreeSet<BString>>,
    pipeline: gix::filter::plumbing::Pipeline,
}

// gix-filter reads this private native selection positionally during conversion.
const NATIVE_ATTRIBUTES: [&str; 6] = [
    "crlf",
    "ident",
    "filter",
    "eol",
    "text",
    "working-tree-encoding",
];

fn match_attributes(
    attributes: &gix::attrs::Search,
    collection: &gix::attrs::search::MetadataCollection,
    case: gix::glob::pattern::Case,
    path: &BStr,
    outcome: &mut gix::attrs::search::Outcome,
) {
    // A reused outcome must drop every previous path's assignments while
    // retaining the pipeline's native attribute order and macro metadata.
    outcome.initialize(collection);
    attributes.pattern_matching_relative_path(path, case, Some(false), outcome);
}

fn add_attributes(
    search: &mut gix::attrs::Search,
    collection: &mut gix::attrs::search::MetadataCollection,
    bytes: &[u8],
    source: PathBuf,
    root: Option<&Path>,
    macros: bool,
) -> Result<(), GitError> {
    if std::str::from_utf8(bytes).is_err() || bytes.contains(&0) {
        return Err(incomplete(
            "working tree status is incomplete: Git attribute metadata is not valid text",
        ));
    }
    // The search parser logs rejected input and skips it; validate every rule
    // and assignment first so no raw control bytes can reach diagnostics.
    for line in gix::attrs::parse(bytes) {
        let (_, assignments, _) = line.map_err(|_| {
            incomplete("working tree status is incomplete: a Git attribute rule is invalid")
        })?;
        for assignment in assignments {
            assignment.map_err(|_| {
                incomplete(
                    "working tree status is incomplete: a Git attribute assignment is invalid",
                )
            })?;
        }
    }
    search.add_patterns_buffer(bytes, source, root, collection, macros);
    Ok(())
}

fn normalization(
    repo: &gix::Repository,
    workdir: &Path,
    paths: &BTreeSet<BString>,
    index: &gix::index::State,
    controls: &mut Controls,
    ignore_case: bool,
) -> Result<Normalization, GitError> {
    let config = repo.config_snapshot();
    let auto_crlf = config
        .string("core.autocrlf")
        .map(|value| gix::config::tree::Core::AUTO_CRLF.try_into_autocrlf(value.as_bstr()))
        .transpose()
        .map_err(|_| incomplete("working tree status is incomplete: core.autocrlf is invalid"))?
        .unwrap_or_default();
    let eol = config
        .string("core.eol")
        .map(|value| gix::config::tree::Core::EOL.try_into_eol(value.as_bstr()))
        .transpose()
        .map_err(|_| incomplete("working tree status is incomplete: core.eol is invalid"))?;
    let mut search = gix::attrs::Search::default();
    let mut collection = gix::attrs::search::MetadataCollection::default();
    add_attributes(
        &mut search,
        &mut collection,
        b"[attr]binary -diff -merge -text",
        PathBuf::from("[builtin]"),
        None,
        true,
    )?;
    let info = repo.common_dir().join("info/attributes");
    if let Some((source, bytes)) = configured_control(
        repo,
        "core.attributesFile",
        &[workdir.join(".gitattributes"), info.clone()],
        controls,
    )? {
        add_attributes(&mut search, &mut collection, &bytes, source, None, true)?;
    }
    let mut directories = BTreeSet::from([workdir.to_path_buf()]);
    for path in paths {
        let mut current = workdir.join(gix::path::from_bstr(path.as_bstr()));
        while let Some(parent) = current
            .parent()
            .filter(|parent| parent.starts_with(workdir))
        {
            directories.insert(parent.to_path_buf());
            current = parent.to_path_buf();
            if current == workdir {
                break;
            }
        }
    }
    let mut directories: Vec<_> = directories.into_iter().collect();
    directories.sort_by_key(|path| (path.components().count(), path.clone()));
    for directory in directories {
        let source = directory.join(".gitattributes");
        let mut bytes = controls.read(workdir, &source)?;
        // Git uses the indexed attribute file when it is absent on disk.
        if bytes.is_none() {
            let relative = source.strip_prefix(workdir).map_err(|_| {
                incomplete(
                    "working tree status is incomplete: attribute path is outside the repository",
                )
            })?;
            let raw = gix::path::to_unix_separators_on_windows(gix::path::into_bstr(relative));
            if let Some(entry) = index
                .entries()
                .iter()
                .find(|entry| entry.stage_raw() == 0 && entry.path(index) == raw.as_ref())
            {
                if !matches!(
                    entry.mode,
                    gix::index::entry::Mode::FILE | gix::index::entry::Mode::FILE_EXECUTABLE
                ) {
                    return Err(incomplete(
                        "working tree status is incomplete: indexed Git attributes are not a regular file",
                    ));
                }
                let mut captured = Vec::new();
                bounded_blob(&repo.objects, &entry.id, CONTROL_FILE_LIMIT, &mut captured).map_err(|_| incomplete("working tree status is incomplete: indexed Git attributes could not be captured within their limit"))?;
                // Replace the remembered absence without counting a file twice.
                controls.files.remove(&source);
                bytes = controls.remember(&source, Some(captured))?;
            }
        }
        if let Some(bytes) = bytes {
            add_attributes(
                &mut search,
                &mut collection,
                &bytes,
                source,
                Some(workdir),
                directory == workdir,
            )?;
        }
    }
    // Repository info attributes override every directory rule.
    if let Some(bytes) = controls.read(repo.common_dir(), &info)? {
        add_attributes(&mut search, &mut collection, &bytes, info, None, true)?;
    }
    let case = if ignore_case {
        gix::glob::pattern::Case::Fold
    } else {
        gix::glob::pattern::Case::Sensitive
    };
    let mut normalized = BTreeSet::new();
    let mut outcome = gix::attrs::search::Outcome::default();
    outcome.initialize_with_selection(&collection, NATIVE_ATTRIBUTES);
    for path in paths {
        match_attributes(&search, &collection, case, path.as_bstr(), &mut outcome);
        for matched in outcome.iter_selected() {
            let name = matched.assignment.name.as_str();
            let state = matched.assignment.state;
            if (matches!(name, "filter" | "ident") && state.is_set())
                || (name == "working-tree-encoding"
                    && !matches!(state, gix::attrs::StateRef::Unspecified))
            {
                return Err(incomplete(
                    "working tree status is incomplete: selected Git filter, encoding or ident attributes are not supported",
                ));
            }
            if matches!(name, "crlf" | "text" | "eol") && matched.location.source.is_some() {
                normalized.insert(path.clone());
            }
        }
        if auto_crlf != gix::filter::plumbing::eol::AutoCrlf::Disabled {
            normalized.insert(path.clone());
        }
    }
    Ok(Normalization {
        attributes: Arc::new(search),
        collection: Arc::new(collection),
        case,
        normalized: Arc::new(normalized),
        pipeline: gix::filter::plumbing::Pipeline::new(
            Default::default(),
            gix::filter::plumbing::pipeline::Options {
                drivers: Vec::new(),
                eol_config: gix::filter::plumbing::eol::Configuration { auto_crlf, eol },
                crlf_roundtrip_check: gix::filter::plumbing::pipeline::CrlfRoundTripCheck::Skip,
                object_hash: repo.object_hash(),
                ..Default::default()
            },
        ),
    })
}

fn member<'a>(
    paths: &'a BTreeSet<BString>,
    path: &BString,
    ignore_case: bool,
) -> Option<&'a BString> {
    if let Some(exact) = paths.get(path) {
        return Some(exact);
    }
    if ignore_case {
        paths
            .iter()
            .find(|candidate| candidate.eq_ignore_ascii_case(path))
    } else {
        None
    }
}

#[allow(clippy::too_many_arguments)]
fn disk_paths(
    workdir: &Path,
    root: Option<&RepoPath>,
    policy: &ExclusionPolicy,
    options: &WalkOptions,
    tracked: &BTreeSet<BString>,
    submodules: &BTreeSet<BString>,
    ignore_case: bool,
    controls: &mut Controls,
    inherited: InheritedIgnores,
) -> Result<BTreeSet<BString>, GitError> {
    if let Some(root) = root {
        match metadata(workdir, root)? {
            Some(value) if value.is_dir() => {}
            _ => {
                return Err(incomplete(
                    "working tree status is incomplete: the project root is not a directory",
                ));
            }
        }
    }
    let directory = root.map_or_else(|| workdir.to_path_buf(), |root| workdir.join(root.as_str()));
    let mut out = BTreeSet::new();
    if inherited.root_ignored {
        return Ok(out);
    }
    let mut stack = vec![(directory, inherited.rules)];
    while let Some((directory, mut rules)) = stack.pop() {
        if options.respect_gitignore {
            let source = directory.join(".gitignore");
            if let Some(bytes) = controls.read(workdir, &source)? {
                rules.push(ignore_rules(&directory, &source, &bytes, ignore_case)?);
            }
        }
        let entries = std::fs::read_dir(&directory).map_err(|_| {
            incomplete(
                "working tree status is incomplete: a project directory could not be enumerated",
            )
        })?;
        for entry in entries {
            let entry = entry.map_err(|_| {
                incomplete("working tree status is incomplete: a directory entry could not be read")
            })?;
            let full = entry.path();
            let Some(raw) = full
                .strip_prefix(workdir)
                .ok()
                .and_then(|path| path.to_str())
            else {
                continue;
            };
            let Ok(path) = RepoPath::new(raw.replace('\\', "/")) else {
                continue;
            };
            if !permitted(&path, root, policy) {
                continue;
            }
            let raw = BString::from(path.as_str());
            if member(submodules, &raw, ignore_case).is_some() {
                continue;
            }
            let tracked_path = member(tracked, &raw, ignore_case);
            if let Some(tracked_path) = tracked_path {
                let Some(canonical) = repo_path(tracked_path.as_ref()) else {
                    continue;
                };
                if !permitted(&canonical, root, policy) {
                    continue;
                }
            }
            let kind = entry.file_type().map_err(|_| {
                incomplete("working tree status is incomplete: file type could not be read")
            })?;
            if tracked_path.is_none() && ignored(&full, kind.is_dir(), &rules) {
                continue;
            }
            if kind.is_dir() {
                stack.push((full, rules.clone()));
            } else {
                let Some(value) = metadata(workdir, &path)? else {
                    continue;
                };
                check_file(&value, options)?;
                out.insert(tracked_path.cloned().unwrap_or(raw));
            }
        }
    }
    Ok(out)
}

fn literal_search(
    paths: &BTreeSet<BString>,
    workdir: &Path,
) -> Result<gix::pathspec::Search, GitError> {
    gix::pathspec::Search::from_specs(
        paths.iter().map(|path| {
            gix::pathspec::Pattern::from_literal(path, gix::pathspec::MagicSignature::TOP)
        }),
        None,
        workdir,
    )
    .map_err(|_| {
        incomplete(
            "working tree status is incomplete: literal path selection could not be constructed",
        )
    })
}

#[derive(Clone)]
struct NoSubmodules;

impl gix::status::plumbing::index_as_worktree::traits::SubmoduleStatus for NoSubmodules {
    type Output = ();

    fn status(&mut self, _entry: &gix::index::Entry, _path: &BStr) -> gix::ExnResult<Option<()>> {
        Ok(None)
    }
}

#[derive(Clone)]
struct ScopedHash {
    paths: Arc<BTreeSet<BString>>,
    index: Arc<gix::index::State>,
    limit: u64,
    normalization: Normalization,
    objects: gix::OdbHandle,
    approved_blobs: Arc<BTreeSet<gix::hash::ObjectId>>,
}

impl gix::status::plumbing::index_as_worktree::traits::CompareBlobs for ScopedHash {
    type Output = ();

    fn compare_blobs<'a, 'b>(
        &mut self,
        entry: &gix::index::Entry,
        size: u64,
        data: impl gix::status::plumbing::index_as_worktree::traits::ReadData<'a>,
        buffer: &mut Vec<u8>,
    ) -> gix::ExnResult<Option<()>> {
        // Literal pathspecs can match descendants. The exact metadata set is
        // therefore checked again at the last boundary before any file opens.
        if !self.paths.contains(entry.path(&self.index)) || size > self.limit {
            return Err(gix::error::message(
                "working tree status is incomplete: a file is outside the approved read set",
            )
            .raise_erased());
        }
        let path = entry.path(&self.index);
        if !self.normalization.normalized.contains(path)
            && u64::from(entry.stat.size) != size
            && (entry.id.is_empty_blob() || entry.stat.size != 0)
        {
            return Ok(Some(()));
        }
        buffer.clear();
        data.stream_worktree_file()?
            .take(self.limit.saturating_add(1))
            .read_to_end(buffer)
            .map_err(gix::hash::io::from_std_io)?;
        if buffer.len() as u64 > self.limit {
            return Err(gix::error::message(
                "working tree status is incomplete: a file grew beyond max_file_bytes",
            )
            .raise_erased());
        }
        let attributes = &self.normalization.attributes;
        let collection = &self.normalization.collection;
        let case = self.normalization.case;
        let objects = &self.objects;
        let approved = &self.approved_blobs;
        let limit = self.limit;
        let converted = self.normalization.pipeline.convert_to_git(
            buffer.as_slice(), &gix::path::from_bstr(path),
                &mut |requested, out| {
                    match_attributes(attributes, collection, case, requested, out);
                },
            &mut |old| {
                if !approved.contains(&entry.id) {
                    return Err(gix::error::message("working tree status is incomplete: an index blob is outside the approved read set").raise_erased());
                }
                bounded_blob(objects, &entry.id, limit, old)
            },
        ).map_err(|_| gix::error::message("working tree status is incomplete: native Git normalization could not be completed").raise_erased())?;
        use gix::filter::plumbing::pipeline::convert::ToGitOutcome;
        let bytes = match converted {
            ToGitOutcome::Unchanged(bytes) | ToGitOutcome::Buffer(bytes) => bytes,
            ToGitOutcome::Process(_) => {
                return Err(gix::error::message(
                    "working tree status is incomplete: external Git filters are not supported",
                )
                .raise_erased());
            }
        };
        let id = gix::objs::compute_hash(entry.id.kind(), gix::objs::Kind::Blob, bytes)
            .map_err(gix::hash::io::from_hasher)?;
        Ok((id != entry.id).then_some(()))
    }
}

/// Computes `HEAD` → worktree changes after filtering metadata by root,
/// built-in exclusions and project policy. Does not update the disk index.
/// Bounded same-repository controls determine ignore and native EOL rules.
/// Capturing controls does not widen source candidates. External controls,
/// clean filters, worktree encodings and ident are rejected explicitly.
pub(super) fn working_changes_scoped(
    repo: &gix::Repository,
    root: Option<&RepoPath>,
    policy: &ExclusionPolicy,
    options: &WalkOptions,
) -> Result<Vec<Change>, GitError> {
    working_changes_with_outcome(repo, root, policy, options).map(|(changes, _)| changes)
}

fn working_changes_with_outcome(
    repo: &gix::Repository,
    root: Option<&RepoPath>,
    policy: &ExclusionPolicy,
    options: &WalkOptions,
) -> Result<
    (
        Vec<Change>,
        Option<gix::status::plumbing::index_as_worktree::Outcome>,
    ),
    GitError,
> {
    let workdir = repo
        .workdir()
        .ok_or_else(|| incomplete("working tree status requires a worktree"))?;
    if root.is_some_and(|root| ExclusionPolicy::builtin().check(root).is_some()) {
        return Ok((Vec::new(), None));
    }
    let index = repo
        .index_or_load_from_head_or_empty()
        .map_err(failed("status index"))?;
    let head = repo
        .head_tree_id_or_empty()
        .map_err(failed("status head"))?;
    let head_index = repo
        .index_from_tree(&head)
        .map_err(failed("status head tree"))?;
    let mut tracked = BTreeSet::new();
    let mut submodules = BTreeSet::new();
    let mut paths = BTreeSet::new();
    for state in [&*index, &head_index] {
        for entry in state.entries() {
            let raw = entry.path(state);
            let Some(path) = repo_path(raw) else {
                continue;
            };
            if !permitted(&path, root, policy) {
                continue;
            }
            if is_submodule(entry.mode) {
                submodules.insert(raw.to_owned());
                continue;
            }
            if let Some(value) = metadata(workdir, &path)? {
                if entry.mode == gix::index::entry::Mode::SYMLINK {
                    return Err(incomplete(
                        "working tree status is incomplete: tracked symbolic links are not supported",
                    ));
                }
                check_file(&value, options)?;
            }
            paths.insert(raw.to_owned());
        }
    }
    for entry in index.entries() {
        if entry.stage_raw() == 0 {
            tracked.insert(entry.path(&index).to_owned());
        }
    }
    let ignore_case = repo
        .filesystem_options()
        .map_err(failed("status filesystem options"))?
        .ignore_case;
    let mut controls = Controls::default();
    let inherited = if options.respect_gitignore {
        inherited_ignores(repo, workdir, root, &mut controls, ignore_case)?
    } else {
        InheritedIgnores::default()
    };
    let disk = disk_paths(
        workdir,
        root,
        policy,
        options,
        &tracked,
        &submodules,
        ignore_case,
        &mut controls,
        inherited,
    )?;
    paths.extend(disk.iter().cloned());
    // An empty gix pathspec means the entire repository, never "nothing".
    if paths.is_empty() {
        return Ok((Vec::new(), None));
    }
    let normalization = normalization(repo, workdir, &paths, &index, &mut controls, ignore_case)?;
    let approved_blobs = index
        .entries()
        .iter()
        .filter(|entry| paths.contains(entry.path(&index)))
        .map(|entry| entry.id)
        .collect();
    let mut status_index = (**index).clone();
    // Stat equality cannot prove normalized equality after control rules
    // change. Invalidate only the in-memory stat cache for affected entries.
    for entry in status_index.entries_mut() {
        if normalization.normalized.contains(entry.path(&index)) {
            entry.stat = Default::default();
        }
    }
    let mut search = literal_search(&paths, workdir)?;
    let mut states: BTreeMap<BString, PathState> = BTreeMap::new();
    gix::diff::index::<gix::Repository>(
        &head_index,
        &index,
        |change| {
            record_tree_index(&change, &mut states);
            Ok(std::ops::ControlFlow::Continue(()))
        },
        None,
        &mut search,
        &mut |_, _, _, _| false,
    )
    .map_err(|error| failed("scoped staged status")(error.into_error()))?;
    for path in disk {
        if !tracked.contains(&path) {
            states.entry(path).or_default().unstaged = Some(Unstaged::Untracked);
        }
    }
    let attributes = gix::worktree::Stack::new(
        workdir.to_path_buf(),
        gix::worktree::stack::State::AttributesStack(Default::default()),
        gix::glob::pattern::Case::Sensitive,
        Vec::new(),
        Vec::new(),
    );
    let mut collector = gix::status::plumbing::index_as_worktree::Recorder::<(), ()>::default();
    let should_interrupt = AtomicBool::new(false);
    let outcome = gix::status::plumbing::index_as_worktree(
        &status_index,
        workdir,
        &mut collector,
        ScopedHash {
            paths: Arc::new(paths.clone()),
            index: Arc::new(status_index.clone()),
            limit: options.max_file_bytes,
            normalization,
            objects: repo.objects.clone(),
            approved_blobs: Arc::new(approved_blobs),
        },
        NoSubmodules,
        repo.objects.clone(),
        &mut gix::progress::Discard,
        gix::status::plumbing::index_as_worktree::Context {
            pathspec: literal_search(&paths, workdir)?,
            stack: attributes,
            filter: gix::filter::plumbing::Pipeline::default(),
            should_interrupt: &should_interrupt,
        },
        gix::status::plumbing::index_as_worktree::Options {
            fs: repo
                .filesystem_options()
                .map_err(failed("status filesystem options"))?,
            stat: repo.stat_options().map_err(failed("status stat options"))?,
            thread_limit: Some(4),
            fscache: false,
        },
    )
    .map_err(|error| failed("scoped tracked status")(error.into_error()))?;
    for record in collector.records {
        if paths.contains(record.relative_path) {
            record_index_worktree(
                record.relative_path,
                record.entry.mode,
                &record.status,
                &mut states,
            );
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
        if !permitted(&path, root, policy) {
            continue;
        }
        changes.push(match kind {
            ChangeKind::Added => Change::Added(path),
            ChangeKind::Modified => Change::Modified(path),
            ChangeKind::Deleted => Change::Deleted(path),
        });
    }
    Ok((changes, Some(outcome)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    #[allow(clippy::unwrap_used)]
    fn frozen_attribute_matcher_resets_paths_and_keeps_native_macro_precedence() {
        let mut attributes = gix::attrs::Search::default();
        let mut collection = gix::attrs::search::MetadataCollection::default();
        let mut rules =
            b"[attr]native text eol=lf\nfirst.txt native\nsecond.txt -text\nunused.txt".to_vec();
        for number in 0..2000 {
            rules.extend_from_slice(format!(" irrelevant_{number}").as_bytes());
        }
        rules.push(b'\n');
        assert!(rules.len() as u64 <= CONTROL_FILE_LIMIT);
        add_attributes(
            &mut attributes,
            &mut collection,
            &rules,
            PathBuf::from("memory-attributes"),
            None,
            true,
        )
        .unwrap();
        add_attributes(
            &mut attributes,
            &mut collection,
            b"first.txt eol=crlf\n",
            PathBuf::from("memory-info-attributes"),
            None,
            true,
        )
        .unwrap();
        let attributes = Arc::new(attributes);
        let collection = Arc::new(collection);
        let worker_attributes = Arc::clone(&attributes);
        let worker_collection = Arc::clone(&collection);
        assert!(Arc::ptr_eq(&attributes, &worker_attributes));
        assert!(Arc::ptr_eq(&collection, &worker_collection));
        let mut outcome = gix::attrs::search::Outcome::default();
        outcome.initialize_with_selection(&collection, NATIVE_ATTRIBUTES);
        let matched = |out: &gix::attrs::search::Outcome, name: &str, value: &str| {
            out.iter_selected().any(|attribute| attribute.assignment.name.as_str() == name && matches!(attribute.assignment.state, gix::attrs::StateRef::Value(actual) if actual.as_bstr() == value))
        };
        match_attributes(
            &worker_attributes,
            &worker_collection,
            gix::glob::pattern::Case::Sensitive,
            "first.txt".into(),
            &mut outcome,
        );
        assert!(
            matched(&outcome, "eol", "crlf"),
            "info priority must survive native macro expansion"
        );
        assert!(
            outcome
                .iter_selected()
                .any(|attribute| attribute.assignment.name.as_str() == "text"
                    && attribute.assignment.state == gix::attrs::StateRef::Set)
        );
        assert_eq!(
            outcome
                .iter_selected()
                .map(|attribute| attribute.assignment.name.as_str().to_owned())
                .collect::<Vec<_>>(),
            NATIVE_ATTRIBUTES
        );
        match_attributes(
            &worker_attributes,
            &worker_collection,
            gix::glob::pattern::Case::Sensitive,
            "second.txt".into(),
            &mut outcome,
        );
        assert!(
            outcome
                .iter_selected()
                .any(|attribute| attribute.assignment.name.as_str() == "text"
                    && attribute.assignment.state == gix::attrs::StateRef::Unset)
        );
        assert!(
            outcome
                .iter_selected()
                .filter(|attribute| attribute.assignment.name.as_str() != "text")
                .all(|attribute| attribute.assignment.state == gix::attrs::StateRef::Unspecified),
            "the first path's EOL assignment leaked into the second"
        );
        match_attributes(
            &worker_attributes,
            &worker_collection,
            gix::glob::pattern::Case::Sensitive,
            "absent.txt".into(),
            &mut outcome,
        );
        assert!(
            outcome
                .iter_selected()
                .all(|attribute| attribute.assignment.state == gix::attrs::StateRef::Unspecified)
        );
        match_attributes(
            &worker_attributes,
            &worker_collection,
            gix::glob::pattern::Case::Sensitive,
            "first.txt".into(),
            &mut outcome,
        );
        assert!(matched(&outcome, "eol", "crlf"));
    }

    #[test]
    #[allow(clippy::unwrap_used)]
    fn metadata_misses_do_not_consume_the_present_control_budget() {
        let mut controls = Controls::default();
        for number in 0..CONTROL_COUNT_LIMIT * 2 {
            controls
                .remember(
                    &PathBuf::from(format!("absent-{number}/.gitattributes")),
                    None,
                )
                .unwrap();
        }
        assert_eq!(controls.present, 0);
        for number in 0..CONTROL_COUNT_LIMIT {
            controls
                .remember(
                    &PathBuf::from(format!("present-{number}/.gitignore")),
                    Some(Vec::new()),
                )
                .unwrap();
        }
        assert_eq!(controls.present, CONTROL_COUNT_LIMIT);
        assert!(
            controls
                .remember(Path::new("one-more/.gitignore"), Some(Vec::new()))
                .is_err()
        );
    }

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

    #[test]
    #[allow(clippy::unwrap_used)]
    fn scoped_status_counts_only_approved_worktree_reads() {
        use std::process::Command;
        let temporary = tempfile::tempdir().unwrap();
        let workdir = temporary.path();
        let global = workdir.join("empty-global-config");
        std::fs::write(&global, b"").unwrap();
        let git = |args: &[&str]| {
            let output = Command::new("git")
                .current_dir(workdir)
                .env("GIT_CONFIG_NOSYSTEM", "1")
                .env("GIT_CONFIG_GLOBAL", &global)
                .env_remove("GIT_DIR")
                .env_remove("GIT_WORK_TREE")
                .env_remove("GIT_INDEX_FILE")
                .args([
                    "-c",
                    "core.autocrlf=false",
                    "-c",
                    "commit.gpgsign=false",
                    "-c",
                    "user.name=Knowell Test",
                    "-c",
                    "user.email=test@example.invalid",
                ])
                .args(args)
                .output()
                .unwrap();
            assert!(output.status.success(), "synthetic git command failed");
        };
        git(&["init", "-q"]);
        std::fs::write(workdir.join(".git/info/exclude"), b"").unwrap();
        std::fs::create_dir(workdir.join("app")).unwrap();
        std::fs::create_dir(workdir.join("sibling")).unwrap();
        let names = [
            "app/[pick].rs",
            "app/same.rs",
            "app/.env",
            "app/private.rs",
            "app/p.rs",
            "sibling/file.rs",
        ];
        for name in names {
            std::fs::write(workdir.join(name), b"let x = 1;\n").unwrap();
        }
        git(&["add", "app", "sibling"]);
        git(&["commit", "-q", "-m", "synthetic read counter base"]);
        for name in names {
            std::fs::write(workdir.join(name), b"let x = 2;\n").unwrap();
        }
        std::fs::write(workdir.join("app/same.rs"), b"let x = 1;\n").unwrap();
        let repo = gix::open_opts(workdir, gix::open::Options::isolated()).unwrap();
        // Force a stat mismatch for each fixture entry without changing its
        // size: every selected entry must reach the actual hash/read boundary.
        let mut index = (**repo.index().unwrap()).clone();
        for entry in index.entries_mut() {
            entry.stat = Default::default();
            entry.stat.size = 11;
        }
        index.write(Default::default()).unwrap();
        let repo = gix::open_opts(workdir, gix::open::Options::isolated()).unwrap();
        let before = std::fs::read(workdir.join(".git/index")).unwrap();
        let policy =
            ExclusionPolicy::with_patterns(&["private.rs".to_owned(), "p.rs".to_owned()]).unwrap();
        let root = RepoPath::new("app").unwrap();
        let (changes, outcome) =
            working_changes_with_outcome(&repo, Some(&root), &policy, &WalkOptions::default())
                .unwrap();
        assert_eq!(
            changes,
            [Change::Modified(RepoPath::new("app/[pick].rs").unwrap())]
        );
        let outcome = outcome.unwrap();
        assert_eq!(outcome.worktree_files_read, 2);
        assert_eq!(outcome.worktree_bytes, 22);
        assert_eq!(outcome.odb_objects_read, 0);
        assert_eq!(std::fs::read(workdir.join(".git/index")).unwrap(), before);
        let deny = ExclusionPolicy::with_patterns(&["**".to_owned()]).unwrap();
        let (changes, outcome) =
            working_changes_with_outcome(&repo, Some(&root), &deny, &WalkOptions::default())
                .unwrap();
        assert!(changes.is_empty());
        assert!(
            outcome.is_none(),
            "empty selection must not enter gix status"
        );
    }
}
