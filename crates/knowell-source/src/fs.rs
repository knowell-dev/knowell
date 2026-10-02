//! Walking a directory tree (a checkout, or any project directory) under
//! the exclusion and secret policies of `knowell-secrets`.
//!
//! [`walk`] enumerates a directory, and for every file applies, in order:
//!
//! 1. path exclusion ([`ExclusionPolicy`]) — **before the file is opened**;
//! 2. the size limit, from metadata — before reading;
//! 3. a binary check (a NUL byte in the first 8 KiB);
//! 4. UTF-8 validation (a leading BOM is stripped);
//! 5. secret redaction ([`knowell_secrets::scan::redact`]).
//!
//! Only the redacted text leaves this crate. The content hash is computed
//! over the original bytes so that change detection still works, and a hash
//! is not reversible. Every file that is not returned is listed with a
//! [`SkipReason`], except `.git` / `.knowell` internals (never reported) and
//! files hidden by ignore rules (the ignore rules are the user's decision).
//! Special files (sockets, FIFOs, devices) are ignored without being opened.
//!
//! The report types ([`WalkReport`], [`SourceFile`], [`SkipReason`]) are
//! shared with [`crate::git::GitRepo::walk_commit`], which reads the same
//! information from git objects instead of a checkout.

use std::fs::File;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use ignore::{DirEntry, WalkBuilder};
use knowell_core::{ContentHash, RepoPath};
use knowell_secrets::scan::Finding;
use knowell_secrets::{Exclusion, ExclusionPolicy};
use serde::{Deserialize, Serialize};

use crate::content::{self, Outcome};

/// Errors that abort a whole walk. Problems with single files are reported
/// as [`SkipReason`]s instead.
#[derive(Debug, thiserror::Error)]
pub enum SourceError {
    /// The root does not exist or is not a directory.
    #[error("source root `{}` is not a directory", .0.display())]
    RootNotDirectory(PathBuf),
    /// The directory walker failed in a way that cannot be attributed to a file.
    #[error("walking the source tree failed ({error_kind})")]
    Walk {
        /// `std::io::ErrorKind` name, or `Other`.
        error_kind: String,
    },
}

/// Options for [`walk`] and [`crate::git::GitRepo::walk_commit`].
///
/// When reading git objects, every *tracked* file is source:
/// `respect_gitignore` does not apply (ignore rules only concern untracked
/// files) and symbolic links are never followed (a link blob only holds the
/// target path), whatever `follow_symlinks` says.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WalkOptions {
    /// Files larger than this many bytes are skipped as
    /// [`SkipReason::TooLarge`]. Default 1 MiB.
    pub max_file_bytes: u64,
    /// Honour `.gitignore` (and `.ignore`) files, also outside git
    /// repositories. Global and parent-directory ignore files are never
    /// consulted, so results do not depend on the machine. Default `true`.
    pub respect_gitignore: bool,
    /// Follow symbolic links. When `false` (the default) a symlink is
    /// reported as [`SkipReason::Symlink`] and never read.
    pub follow_symlinks: bool,
}

impl Default for WalkOptions {
    fn default() -> Self {
        Self {
            max_file_bytes: 1024 * 1024,
            respect_gitignore: true,
            follow_symlinks: false,
        }
    }
}

/// A readable text file, after redaction.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SourceFile {
    /// Path relative to the walk root.
    pub path: RepoPath,
    /// File text **after** secret redaction, without a leading BOM.
    pub text: String,
    /// BLAKE3 hash of the **original** bytes (including any BOM and any
    /// secrets), so that edits to redacted content are still detected.
    pub hash: ContentHash,
    /// Size of the original file in bytes.
    pub size: u64,
    /// Secrets that were redacted; offsets refer to the original text
    /// (after BOM removal), see [`Finding`]. Never contains the secrets.
    pub redactions: Vec<Finding>,
}

/// Why a file or directory was not returned.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SkipReason {
    /// Excluded by path (sensitive file, user pattern, ...). Never opened.
    Excluded(Exclusion),
    /// Larger than [`WalkOptions::max_file_bytes`]; carries the size in bytes.
    TooLarge {
        /// File size in bytes.
        size: u64,
    },
    /// Contains a NUL byte in the first 8 KiB.
    Binary,
    /// Not valid UTF-8.
    NotUtf8,
    /// Could not be read; carries the `std::io::ErrorKind` name only.
    Unreadable {
        /// e.g. `PermissionDenied`.
        error_kind: String,
    },
    /// The path is not a valid [`RepoPath`] (for example not UTF-8). The
    /// reported path is a lossy rendition.
    InvalidPath,
    /// A symbolic link and [`WalkOptions::follow_symlinks`] is off.
    Symlink,
}

impl SkipReason {
    /// Stable snake-case code of the reason (`excluded`, `too_large`,
    /// `binary`, `not_utf8`, `unreadable`, `invalid_path`, `symlink`), the
    /// same text the serialized form uses as its tag. Carries no path, size
    /// or error detail, so it can be stored and compared across runs.
    pub fn as_str(&self) -> &'static str {
        match self {
            SkipReason::Excluded(_) => "excluded",
            SkipReason::TooLarge { .. } => "too_large",
            SkipReason::Binary => "binary",
            SkipReason::NotUtf8 => "not_utf8",
            SkipReason::Unreadable { .. } => "unreadable",
            SkipReason::InvalidPath => "invalid_path",
            SkipReason::Symlink => "symlink",
        }
    }
}

/// Result of reading one file with [`read_file`] or
/// [`crate::git::GitRepo::read_commit_file`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FileRead {
    /// A readable text file, already redacted.
    File(SourceFile),
    /// The file exists but is not returned, with the reason (excluded paths
    /// are reported here without ever being looked up).
    Skipped(SkipReason),
    /// Nothing readable as a file is at the path: it does not exist, it is a
    /// directory, or (on disk) it is a special file such as a socket or FIFO,
    /// which is never opened. A whole-tree walk would not list it either.
    Missing,
}

/// A skipped file (or pruned directory) and the reason.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SkippedFile {
    /// Path relative to the walk root.
    pub path: RepoPath,
    /// Why it was skipped.
    pub reason: SkipReason,
}

/// Result of [`walk`]. Both lists are sorted by path (byte-wise), so the
/// output is identical on every platform and run.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct WalkReport {
    /// Files that were read, sorted by path.
    pub files: Vec<SourceFile>,
    /// Files and pruned directories that were not read, sorted by path.
    pub skipped: Vec<SkippedFile>,
}

fn relative(root: &Path, path: &Path) -> Option<RepoPath> {
    let rel = path.strip_prefix(root).ok()?;
    RepoPath::from_relative(rel).ok()
}

/// A [`RepoPath`] for reporting a path that failed validation: components
/// are rendered lossily and unusable characters replaced. Fails only if even
/// that rendition is invalid, which cannot happen for real directory entries.
fn lossy_path(root: &Path, path: &Path) -> Result<RepoPath, SourceError> {
    let rel = path.strip_prefix(root).unwrap_or(path);
    let joined = rel
        .components()
        .filter_map(|c| match c {
            std::path::Component::Normal(part) => {
                Some(part.to_string_lossy().replace(['\\', '\0'], "?"))
            }
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("/");
    RepoPath::new(joined).map_err(|_| SourceError::Walk {
        error_kind: "InvalidPath".to_owned(),
    })
}

fn kind_name(error: &std::io::Error) -> String {
    format!("{:?}", error.kind())
}

fn error_path(error: &ignore::Error) -> Option<&Path> {
    match error {
        ignore::Error::WithPath { path, .. } => Some(path),
        ignore::Error::WithDepth { err, .. } | ignore::Error::WithLineNumber { err, .. } => {
            error_path(err)
        }
        ignore::Error::Partial(errors) => errors.iter().find_map(error_path),
        _ => None,
    }
}

fn error_kind(error: &ignore::Error) -> String {
    error
        .io_error()
        .map_or_else(|| "Other".to_owned(), kind_name)
}

fn read_entry(entry: &DirEntry, path: RepoPath, options: &WalkOptions) -> Outcome {
    let metadata = match entry.metadata() {
        Ok(m) => m,
        Err(e) => {
            return Outcome::Skip(SkipReason::Unreadable {
                error_kind: error_kind(&e),
            });
        }
    };
    read_bounded(entry.path(), metadata.len(), path, options)
}

/// Size limit from metadata (`known_size`) before opening, then a bounded
/// read (the file may have grown since), then the shared content pipeline.
fn read_bounded(full: &Path, known_size: u64, path: RepoPath, options: &WalkOptions) -> Outcome {
    if known_size > options.max_file_bytes {
        return Outcome::Skip(SkipReason::TooLarge { size: known_size });
    }
    let unreadable = |e: &std::io::Error| {
        Outcome::Skip(SkipReason::Unreadable {
            error_kind: kind_name(e),
        })
    };
    let file = match File::open(full) {
        Ok(f) => f,
        Err(e) => return unreadable(&e),
    };
    let mut bytes = Vec::new();
    let limit = options.max_file_bytes.saturating_add(1);
    if let Err(e) = file.take(limit).read_to_end(&mut bytes) {
        return unreadable(&e);
    }
    let size = bytes.len() as u64;
    if size > options.max_file_bytes {
        return Outcome::Skip(SkipReason::TooLarge { size });
    }
    content::decode(path, &bytes)
}

/// The OS path of `path` below `root`, joined component by component (a
/// [`RepoPath`] never contains `..`, a root or a drive prefix, so the result
/// stays below `root`).
fn os_path(root: &Path, path: &RepoPath) -> PathBuf {
    let mut out = root.to_path_buf();
    for component in path.components() {
        out.push(component);
    }
    out
}

/// Reads one file below `root` under exactly the rules of [`walk`], for
/// callers that know which paths changed (personal overlays, re-reads of a
/// few files) and must not walk the whole tree:
///
/// 1. path exclusion (`policy`) — decided from the path alone, **before the
///    file system is touched** (an excluded path is reported even if it does
///    not exist);
/// 2. symbolic links are [`SkipReason::Symlink`] unless
///    [`WalkOptions::follow_symlinks`] is set;
/// 3. the size limit from metadata, before the file is opened, and a bounded
///    read;
/// 4. the shared binary / UTF-8 / redaction pipeline, so the text, findings
///    and hash equal what [`walk`] and the git reader produce for the same
///    bytes.
///
/// [`WalkOptions::respect_gitignore`] does not apply: the caller names the
/// file explicitly. `.git` / `.knowell` internals are reported as excluded.
///
/// # Errors
/// [`SourceError::RootNotDirectory`] if `root` is not a directory. Problems
/// with the file itself are [`FileRead::Skipped`] or [`FileRead::Missing`].
pub fn read_file(
    root: &Path,
    path: &RepoPath,
    policy: &ExclusionPolicy,
    options: &WalkOptions,
) -> Result<FileRead, SourceError> {
    if !root.is_dir() {
        return Err(SourceError::RootNotDirectory(root.to_path_buf()));
    }
    if let Some(exclusion) = policy.check(path) {
        return Ok(FileRead::Skipped(SkipReason::Excluded(exclusion)));
    }
    let full = os_path(root, path);
    let metadata = match std::fs::symlink_metadata(&full) {
        Ok(m) => m,
        Err(e) if is_absent(&e) => return Ok(FileRead::Missing),
        Err(e) => {
            return Ok(FileRead::Skipped(SkipReason::Unreadable {
                error_kind: kind_name(&e),
            }));
        }
    };
    let metadata = if metadata.file_type().is_symlink() {
        if !options.follow_symlinks {
            return Ok(FileRead::Skipped(SkipReason::Symlink));
        }
        match std::fs::metadata(&full) {
            Ok(m) => m,
            // A dangling link points at nothing readable.
            Err(e) if is_absent(&e) => return Ok(FileRead::Missing),
            Err(e) => {
                return Ok(FileRead::Skipped(SkipReason::Unreadable {
                    error_kind: kind_name(&e),
                }));
            }
        }
    } else {
        metadata
    };
    // Directories and special files are never opened (a FIFO would block).
    if !metadata.is_file() {
        return Ok(FileRead::Missing);
    }
    Ok(
        match read_bounded(&full, metadata.len(), path.clone(), options) {
            Outcome::File(file) => FileRead::File(*file),
            // The file vanished between the metadata read and opening it.
            Outcome::Skip(SkipReason::Unreadable { error_kind })
                if error_kind == kind_name(&std::io::ErrorKind::NotFound.into()) =>
            {
                FileRead::Missing
            }
            Outcome::Skip(reason) => FileRead::Skipped(reason),
        },
    )
}

/// Whether an I/O error means nothing is at the path (also when a parent
/// component is a file, not a directory).
fn is_absent(error: &std::io::Error) -> bool {
    matches!(
        error.kind(),
        std::io::ErrorKind::NotFound | std::io::ErrorKind::NotADirectory
    )
}

/// Walks `root` and reads every allowed text file.
///
/// Hidden files and directories are included (`.github/` is source; a
/// tracked `.env` is *reported* as excluded rather than silently hidden),
/// `.git` is never entered, and excluded directories are pruned whole.
/// `.gitignore` handling follows [`WalkOptions::respect_gitignore`].
///
/// # Errors
/// [`SourceError::RootNotDirectory`] if `root` is not a directory;
/// [`SourceError::Walk`] for walker failures that name no file. Failures on
/// individual files become [`SkipReason`]s.
pub fn walk(
    root: &Path,
    policy: &ExclusionPolicy,
    options: &WalkOptions,
) -> Result<WalkReport, SourceError> {
    if !root.is_dir() {
        return Err(SourceError::RootNotDirectory(root.to_path_buf()));
    }

    let pruned: Arc<Mutex<Vec<SkippedFile>>> = Arc::default();
    let mut builder = WalkBuilder::new(root);
    builder
        .standard_filters(false)
        .git_ignore(options.respect_gitignore)
        .git_exclude(options.respect_gitignore)
        .ignore(options.respect_gitignore)
        .require_git(false)
        .follow_links(options.follow_symlinks);

    let filter_policy = policy.clone();
    let filter_root = root.to_path_buf();
    let filter_pruned = Arc::clone(&pruned);
    builder.filter_entry(move |entry| {
        if entry.depth() == 0 || !entry.file_type().is_some_and(|t| t.is_dir()) {
            return true;
        }
        let Some(rel) = relative(&filter_root, entry.path()) else {
            return true;
        };
        match filter_policy.check_dir(&rel) {
            None => true,
            Some(exclusion) => {
                // `.git` / `.knowell` internals are never reported.
                if exclusion != Exclusion::Internal
                    && let Ok(mut list) = filter_pruned.lock()
                {
                    list.push(SkippedFile {
                        path: rel,
                        reason: SkipReason::Excluded(exclusion),
                    });
                }
                false
            }
        }
    });

    let mut files = Vec::new();
    let mut skipped = Vec::new();
    for item in builder.build() {
        let entry = match item {
            Ok(entry) => entry,
            Err(error) => {
                let Some(path) = error_path(&error) else {
                    return Err(SourceError::Walk {
                        error_kind: error_kind(&error),
                    });
                };
                let path = match relative(root, path) {
                    Some(p) => p,
                    None => lossy_path(root, path)?,
                };
                skipped.push(SkippedFile {
                    path,
                    reason: SkipReason::Unreadable {
                        error_kind: error_kind(&error),
                    },
                });
                continue;
            }
        };
        if entry.depth() == 0 {
            continue;
        }
        let Some(file_type) = entry.file_type() else {
            continue;
        };
        if file_type.is_dir() {
            continue;
        }
        let Some(path) = relative(root, entry.path()) else {
            skipped.push(SkippedFile {
                path: lossy_path(root, entry.path())?,
                reason: SkipReason::InvalidPath,
            });
            continue;
        };
        if let Some(exclusion) = policy.check(&path) {
            if exclusion != Exclusion::Internal {
                skipped.push(SkippedFile {
                    path,
                    reason: SkipReason::Excluded(exclusion),
                });
            }
            continue;
        }
        if file_type.is_symlink() {
            skipped.push(SkippedFile {
                path,
                reason: SkipReason::Symlink,
            });
            continue;
        }
        if !file_type.is_file() {
            continue;
        }
        match read_entry(&entry, path.clone(), options) {
            Outcome::File(file) => files.push(*file),
            Outcome::Skip(reason) => skipped.push(SkippedFile { path, reason }),
        }
    }

    if let Ok(mut list) = pruned.lock() {
        skipped.append(&mut list);
    }
    files.sort_by(|a, b| a.path.cmp(&b.path));
    skipped.sort_by(|a, b| a.path.cmp(&b.path));
    Ok(WalkReport { files, skipped })
}

#[cfg(test)]
mod tests {
    use std::fs;

    use knowell_secrets::SensitiveKind;
    use knowell_secrets::scan::FindingKind;

    use super::*;
    use crate::content::{BINARY_SNIFF_BYTES, UTF8_BOM};

    fn write(root: &Path, rel: &str, bytes: &[u8]) {
        let path = root.join(rel);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        fs::write(path, bytes).unwrap();
    }

    fn canary() -> String {
        format!("KNOWELL_CANARY_{}", "7f3a9c")
    }

    fn token() -> String {
        format!("ghp_{}", "FAKE".repeat(9))
    }

    fn fixture() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        let r = dir.path();
        write(r, ".env", format!("API_TOKEN={}\n", canary()).as_bytes());
        write(r, "config/id_rsa", format!("{}\n", canary()).as_bytes());
        write(r, ".gitignore", b"ignored.txt\nbuild/\n");
        write(r, "ignored.txt", b"hello ignored\n");
        write(r, "build/out.js", b"ignored dir\n");
        write(r, "data.bin", &[0x89, b'P', 0, 1, 2, 3]);
        write(r, "big.txt", &vec![b'a'; 2048]);
        write(
            r,
            "src/client.ts",
            format!("// line1\nconst gh = \"{}\";\n", token()).as_bytes(),
        );
        write(r, ".git/config", format!("[x]\n{}\n", canary()).as_bytes());
        write(r, "sub/.git/HEAD", b"ref: refs/heads/main\n");
        write(r, ".github/workflows/ci.yml", b"name: ci\non: push\n");
        write(r, "README.md", b"# hi\n");
        dir
    }

    fn small_limit() -> WalkOptions {
        WalkOptions {
            max_file_bytes: 1024,
            ..WalkOptions::default()
        }
    }

    fn paths(report: &WalkReport) -> Vec<&str> {
        report.files.iter().map(|f| f.path.as_str()).collect()
    }

    fn skip_of<'a>(report: &'a WalkReport, path: &str) -> Option<&'a SkipReason> {
        report
            .skipped
            .iter()
            .find(|s| s.path.as_str() == path)
            .map(|s| &s.reason)
    }

    #[test]
    fn fixture_walk_end_to_end() {
        let dir = fixture();
        let report = walk(dir.path(), &ExclusionPolicy::builtin(), &small_limit()).unwrap();

        assert_eq!(
            paths(&report),
            [
                ".github/workflows/ci.yml",
                ".gitignore",
                "README.md",
                "src/client.ts"
            ]
        );
        // The canary appears nowhere, neither in text nor in Debug output.
        for f in &report.files {
            assert!(!f.text.contains(&canary()), "{}", f.path);
        }
        assert!(!format!("{report:?}").contains(&canary()));
        assert!(!format!("{report:?}").contains(&token()));

        assert_eq!(
            skip_of(&report, ".env"),
            Some(&SkipReason::Excluded(Exclusion::Sensitive(
                SensitiveKind::EnvFile
            )))
        );
        assert_eq!(
            skip_of(&report, "config/id_rsa"),
            Some(&SkipReason::Excluded(Exclusion::Sensitive(
                SensitiveKind::PrivateKey
            )))
        );
        assert_eq!(skip_of(&report, "data.bin"), Some(&SkipReason::Binary));
        assert_eq!(
            skip_of(&report, "big.txt"),
            Some(&SkipReason::TooLarge { size: 2048 })
        );
        // Gitignored and .git content is absent from both lists.
        for gone in [
            "ignored.txt",
            "build/out.js",
            ".git",
            ".git/config",
            "sub/.git/HEAD",
            "sub/.git",
        ] {
            assert!(skip_of(&report, gone).is_none(), "{gone}");
            assert!(!paths(&report).contains(&gone), "{gone}");
        }
    }

    #[test]
    fn token_is_redacted_with_kind_and_line() {
        let dir = fixture();
        let report = walk(dir.path(), &ExclusionPolicy::builtin(), &small_limit()).unwrap();
        let file = report
            .files
            .iter()
            .find(|f| f.path.as_str() == "src/client.ts")
            .unwrap();
        assert_eq!(file.redactions.len(), 1);
        assert_eq!(file.redactions[0].kind, FindingKind::GithubToken);
        assert_eq!(file.redactions[0].line, 2);
        assert_eq!(
            file.text,
            "// line1\nconst gh = \"[REDACTED:github_token]\";\n"
        );
        // Hash covers the ORIGINAL bytes.
        let original = format!("// line1\nconst gh = \"{}\";\n", token());
        assert_eq!(file.hash, ContentHash::of(original.as_bytes()));
        assert_eq!(file.size, original.len() as u64);
    }

    #[test]
    fn gitignore_can_be_disabled() {
        let dir = fixture();
        let options = WalkOptions {
            respect_gitignore: false,
            ..small_limit()
        };
        let report = walk(dir.path(), &ExclusionPolicy::builtin(), &options).unwrap();
        assert!(paths(&report).contains(&"ignored.txt"));
        assert!(paths(&report).contains(&"build/out.js"));
        // Exclusions are not affected, and .git is still never entered.
        assert!(skip_of(&report, ".env").is_some());
        assert!(!paths(&report).iter().any(|p| p.starts_with(".git/")));
    }

    #[test]
    fn output_is_sorted_and_deterministic() {
        let dir = fixture();
        let a = walk(dir.path(), &ExclusionPolicy::builtin(), &small_limit()).unwrap();
        let b = walk(dir.path(), &ExclusionPolicy::builtin(), &small_limit()).unwrap();
        assert_eq!(a, b);
        assert!(a.files.windows(2).all(|w| w[0].path < w[1].path));
        assert!(a.skipped.windows(2).all(|w| w[0].path <= w[1].path));
    }

    #[test]
    fn user_patterns_prune_directories() {
        let dir = fixture();
        write(dir.path(), "vendor/lib/a.js", b"x\n");
        let policy = ExclusionPolicy::with_patterns(&["vendor".to_owned()]).unwrap();
        let report = walk(dir.path(), &policy, &small_limit()).unwrap();
        assert_eq!(
            skip_of(&report, "vendor"),
            Some(&SkipReason::Excluded(Exclusion::Pattern("vendor".into())))
        );
        assert!(!paths(&report).iter().any(|p| p.starts_with("vendor")));
        assert!(skip_of(&report, "vendor/lib/a.js").is_none());
    }

    #[test]
    fn excluded_key_directory_is_pruned_without_reading() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), ".ssh/known_hosts", b"host\n");
        write(dir.path(), "a.txt", b"a\n");
        let report = walk(
            dir.path(),
            &ExclusionPolicy::builtin(),
            &WalkOptions::default(),
        )
        .unwrap();
        assert_eq!(paths(&report), ["a.txt"]);
        assert_eq!(
            skip_of(&report, ".ssh"),
            Some(&SkipReason::Excluded(Exclusion::Sensitive(
                SensitiveKind::PrivateKey
            )))
        );
    }

    #[test]
    fn bom_is_stripped_and_hash_is_over_original() {
        let dir = tempfile::tempdir().unwrap();
        let mut bytes = UTF8_BOM.to_vec();
        bytes.extend_from_slice(b"hello\r\nworld\r\n");
        write(dir.path(), "bom.txt", &bytes);
        let report = walk(
            dir.path(),
            &ExclusionPolicy::builtin(),
            &WalkOptions::default(),
        )
        .unwrap();
        let f = &report.files[0];
        assert_eq!(f.text, "hello\r\nworld\r\n");
        assert_eq!(f.hash, ContentHash::of(&bytes));
        assert_eq!(f.size, bytes.len() as u64);
    }

    #[test]
    fn not_utf8_and_empty_and_binary_edges() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "latin1.txt", &[b'c', b'a', b'f', 0xE9]);
        write(dir.path(), "empty.txt", b"");
        // NUL after the sniff window is not detected as binary by the sniffer,
        // but is still valid UTF-8 text and therefore readable.
        let mut late_nul = vec![b'a'; BINARY_SNIFF_BYTES + 10];
        late_nul.push(0);
        write(dir.path(), "late_nul.txt", &late_nul);
        write(dir.path(), "utf16.txt", &[0xFF, 0xFE, b'a', 0, b'b', 0]);
        let report = walk(
            dir.path(),
            &ExclusionPolicy::builtin(),
            &WalkOptions::default(),
        )
        .unwrap();
        assert_eq!(skip_of(&report, "latin1.txt"), Some(&SkipReason::NotUtf8));
        assert_eq!(skip_of(&report, "utf16.txt"), Some(&SkipReason::Binary));
        let empty = report
            .files
            .iter()
            .find(|f| f.path.as_str() == "empty.txt")
            .unwrap();
        assert_eq!(empty.text, "");
        assert_eq!(empty.size, 0);
        assert!(paths(&report).contains(&"late_nul.txt"));
    }

    #[test]
    fn size_limit_is_inclusive() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "exact.txt", &[b'a'; 100]);
        write(dir.path(), "over.txt", &[b'a'; 101]);
        let options = WalkOptions {
            max_file_bytes: 100,
            ..WalkOptions::default()
        };
        let report = walk(dir.path(), &ExclusionPolicy::builtin(), &options).unwrap();
        assert_eq!(paths(&report), ["exact.txt"]);
        assert_eq!(
            skip_of(&report, "over.txt"),
            Some(&SkipReason::TooLarge { size: 101 })
        );
    }

    #[test]
    fn hostile_content_is_handled() {
        let dir = tempfile::tempdir().unwrap();
        let long_line = format!("password = \"{}\"\n", "Ab1Cd2Ef3".repeat(100_000));
        write(dir.path(), "long.txt", long_line.as_bytes());
        write(dir.path(), "uni.txt", "héllo ✓ 日本語 🔑\r\n".as_bytes());
        let options = WalkOptions {
            max_file_bytes: 10 * 1024 * 1024,
            ..WalkOptions::default()
        };
        let report = walk(dir.path(), &ExclusionPolicy::builtin(), &options).unwrap();
        let long = report
            .files
            .iter()
            .find(|f| f.path.as_str() == "long.txt")
            .unwrap();
        assert_eq!(long.text, "password = \"[REDACTED:generic_secret]\"\n");
        let uni = report
            .files
            .iter()
            .find(|f| f.path.as_str() == "uni.txt")
            .unwrap();
        assert_eq!(uni.text, "héllo ✓ 日本語 🔑\r\n");
    }

    #[test]
    fn env_variants_never_appear_in_any_text() {
        let dir = tempfile::tempdir().unwrap();
        for name in [
            ".env",
            ".env.local",
            ".env.example",
            "app/prod.env",
            "terraform.tfstate",
            "kubeconfig",
            "creds/credentials",
        ] {
            write(dir.path(), name, format!("v={}\n", canary()).as_bytes());
        }
        write(dir.path(), "ok.txt", b"fine\n");
        let report = walk(
            dir.path(),
            &ExclusionPolicy::builtin(),
            &WalkOptions::default(),
        )
        .unwrap();
        assert_eq!(paths(&report), ["ok.txt"]);
        assert_eq!(report.skipped.len(), 7);
        assert!(!format!("{report:?}").contains(&canary()));
    }

    #[test]
    fn root_must_be_a_directory() {
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("nope");
        assert!(matches!(
            walk(
                &missing,
                &ExclusionPolicy::builtin(),
                &WalkOptions::default()
            ),
            Err(SourceError::RootNotDirectory(_))
        ));
        write(dir.path(), "f.txt", b"x");
        assert!(matches!(
            walk(
                &dir.path().join("f.txt"),
                &ExclusionPolicy::builtin(),
                &WalkOptions::default()
            ),
            Err(SourceError::RootNotDirectory(_))
        ));
    }

    #[test]
    fn empty_directory_yields_empty_report() {
        let dir = tempfile::tempdir().unwrap();
        let report = walk(
            dir.path(),
            &ExclusionPolicy::builtin(),
            &WalkOptions::default(),
        )
        .unwrap();
        assert_eq!(report, WalkReport::default());
    }

    #[test]
    fn nested_gitignore_and_negation() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "a/.gitignore", b"*.tmp\n!keep.tmp\n");
        write(dir.path(), "a/x.tmp", b"x");
        write(dir.path(), "a/keep.tmp", b"k");
        write(dir.path(), "b/x.tmp", b"x");
        let report = walk(
            dir.path(),
            &ExclusionPolicy::builtin(),
            &WalkOptions::default(),
        )
        .unwrap();
        assert_eq!(paths(&report), ["a/.gitignore", "a/keep.tmp", "b/x.tmp"]);
    }

    #[test]
    fn engine_state_dir_is_never_reported() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), ".knowell/index/seg", b"x");
        write(dir.path(), "a.txt", b"a");
        let report = walk(
            dir.path(),
            &ExclusionPolicy::builtin(),
            &WalkOptions::default(),
        )
        .unwrap();
        assert_eq!(paths(&report), ["a.txt"]);
        assert!(report.skipped.is_empty());
    }

    #[test]
    fn symlinks_are_reported_not_followed() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "real.txt", b"real\n");
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(dir.path().join("real.txt"), dir.path().join("link.txt"))
                .unwrap();
            std::os::unix::fs::symlink(dir.path().join("real.txt"), dir.path().join(".env"))
                .unwrap();
            let report = walk(
                dir.path(),
                &ExclusionPolicy::builtin(),
                &WalkOptions::default(),
            )
            .unwrap();
            assert_eq!(paths(&report), ["real.txt"]);
            assert_eq!(skip_of(&report, "link.txt"), Some(&SkipReason::Symlink));
            assert!(matches!(
                skip_of(&report, ".env"),
                Some(SkipReason::Excluded(_))
            ));
        }
        #[cfg(not(unix))]
        {
            let report = walk(
                dir.path(),
                &ExclusionPolicy::builtin(),
                &WalkOptions::default(),
            )
            .unwrap();
            assert_eq!(paths(&report), ["real.txt"]);
        }
    }

    #[cfg(unix)]
    #[test]
    fn non_utf8_paths_are_rejected_and_lossily_reported() {
        use std::ffi::OsStr;
        use std::os::unix::ffi::OsStrExt;

        // APFS rejects invalid UTF-8 names at creation, before the walker can
        // inspect them. Exercise path handling without requiring such a file.
        let root = Path::new("/source");
        let path = root.join(OsStr::from_bytes(b"dir/bad\xff.txt"));
        assert!(relative(root, &path).is_none());
        assert_eq!(
            lossy_path(root, &path).unwrap().as_str(),
            "dir/bad\u{fffd}.txt"
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn non_utf8_names_are_skipped_not_fatal() {
        use std::ffi::OsStr;
        use std::os::unix::ffi::OsStrExt;
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "ok.txt", b"ok");
        let bad = dir.path().join(OsStr::from_bytes(b"bad\xff.txt"));
        fs::write(bad, b"x").unwrap();
        let report = walk(
            dir.path(),
            &ExclusionPolicy::builtin(),
            &WalkOptions::default(),
        )
        .unwrap();
        assert_eq!(paths(&report), ["ok.txt"]);
        assert!(
            report
                .skipped
                .iter()
                .any(|s| s.reason == SkipReason::InvalidPath)
        );
    }

    #[test]
    fn lossy_paths_are_valid() {
        let p = lossy_path(Path::new("/r"), Path::new("/r/a\\b/c")).unwrap();
        assert!(!p.as_str().contains('\\'));
    }

    fn rp(s: &str) -> RepoPath {
        RepoPath::new(s).unwrap()
    }

    #[test]
    fn read_file_matches_the_walker() {
        let dir = fixture();
        let policy = ExclusionPolicy::builtin();
        let report = walk(dir.path(), &policy, &small_limit()).unwrap();
        for walked in &report.files {
            let FileRead::File(read) =
                read_file(dir.path(), &walked.path, &policy, &small_limit()).unwrap()
            else {
                panic!("{} should be readable", walked.path);
            };
            assert_eq!(&read, walked, "{}", walked.path);
        }
        // Redaction and the hash of the original bytes, as in the walker.
        let FileRead::File(client) =
            read_file(dir.path(), &rp("src/client.ts"), &policy, &small_limit()).unwrap()
        else {
            panic!("expected a file");
        };
        assert!(!client.text.contains(&token()));
        let original = format!("// line1\nconst gh = \"{}\";\n", token());
        assert_eq!(client.hash, ContentHash::of(original.as_bytes()));
        for skipped in &report.skipped {
            let read = read_file(dir.path(), &skipped.path, &policy, &small_limit()).unwrap();
            assert_eq!(
                read,
                FileRead::Skipped(skipped.reason.clone()),
                "{}",
                skipped.path
            );
        }
    }

    #[test]
    fn read_file_decides_exclusion_before_touching_the_disk() {
        let dir = tempfile::tempdir().unwrap();
        let policy = ExclusionPolicy::builtin();
        // Neither file exists: the decision is the path's alone.
        for path in [".env", "config/id_rsa", ".git/config"] {
            assert!(
                matches!(
                    read_file(dir.path(), &rp(path), &policy, &WalkOptions::default()).unwrap(),
                    FileRead::Skipped(SkipReason::Excluded(_))
                ),
                "{path}"
            );
        }
        // An existing secret file is reported without its content.
        write(
            dir.path(),
            ".env",
            format!("TOKEN={}\n", canary()).as_bytes(),
        );
        let read = read_file(dir.path(), &rp(".env"), &policy, &WalkOptions::default()).unwrap();
        assert!(!format!("{read:?}").contains(&canary()));
    }

    #[test]
    fn read_file_limits_binary_utf8_missing_and_directories() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "big.txt", &[b'a'; 100]);
        write(dir.path(), "exact.txt", &[b'a'; 10]);
        write(dir.path(), "bin", b"a\0b");
        write(dir.path(), "latin1", &[0xC3, 0x28]);
        write(dir.path(), "d/inner.txt", b"x");
        write(dir.path(), "empty.txt", b"");
        let policy = ExclusionPolicy::builtin();
        let options = WalkOptions {
            max_file_bytes: 10,
            ..WalkOptions::default()
        };
        let read = |p: &str| read_file(dir.path(), &rp(p), &policy, &options).unwrap();
        assert_eq!(
            read("big.txt"),
            FileRead::Skipped(SkipReason::TooLarge { size: 100 })
        );
        assert!(matches!(read("exact.txt"), FileRead::File(f) if f.size == 10));
        assert_eq!(read("bin"), FileRead::Skipped(SkipReason::Binary));
        assert_eq!(read("latin1"), FileRead::Skipped(SkipReason::NotUtf8));
        assert_eq!(read("gone"), FileRead::Missing);
        assert_eq!(read("d"), FileRead::Missing);
        // A file where a directory was expected.
        assert_eq!(read("exact.txt/below"), FileRead::Missing);
        assert!(matches!(read("empty.txt"), FileRead::File(f) if f.text.is_empty()));
        let bom = [&UTF8_BOM[..], b"hi"].concat();
        write(dir.path(), "bom.txt", &bom);
        let FileRead::File(file) = read("bom.txt") else {
            panic!("expected a file");
        };
        assert_eq!(file.text, "hi");
        assert_eq!(file.hash, ContentHash::of(&bom));
    }

    #[test]
    fn read_file_needs_a_directory_root() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "f.txt", b"x");
        let policy = ExclusionPolicy::builtin();
        for root in [dir.path().join("nope"), dir.path().join("f.txt")] {
            assert!(matches!(
                read_file(&root, &rp("a"), &policy, &WalkOptions::default()),
                Err(SourceError::RootNotDirectory(_))
            ));
        }
    }

    #[cfg(unix)]
    #[test]
    fn read_file_reports_symlinks_unless_following() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "real.txt", b"real\n");
        std::os::unix::fs::symlink(dir.path().join("real.txt"), dir.path().join("link.txt"))
            .unwrap();
        std::os::unix::fs::symlink(dir.path().join("nowhere"), dir.path().join("dangling"))
            .unwrap();
        let policy = ExclusionPolicy::builtin();
        let mut options = WalkOptions::default();
        assert_eq!(
            read_file(dir.path(), &rp("link.txt"), &policy, &options).unwrap(),
            FileRead::Skipped(SkipReason::Symlink)
        );
        options.follow_symlinks = true;
        assert!(matches!(
            read_file(dir.path(), &rp("link.txt"), &policy, &options).unwrap(),
            FileRead::File(f) if f.text == "real\n"
        ));
        assert_eq!(
            read_file(dir.path(), &rp("dangling"), &policy, &options).unwrap(),
            FileRead::Missing
        );
    }

    #[test]
    fn skip_reason_codes_are_stable() {
        let reasons = [
            SkipReason::Excluded(Exclusion::Internal),
            SkipReason::TooLarge { size: 1 },
            SkipReason::Binary,
            SkipReason::NotUtf8,
            SkipReason::Unreadable {
                error_kind: "PermissionDenied".into(),
            },
            SkipReason::InvalidPath,
            SkipReason::Symlink,
        ];
        let codes: Vec<&str> = reasons.iter().map(SkipReason::as_str).collect();
        assert_eq!(
            codes,
            [
                "excluded",
                "too_large",
                "binary",
                "not_utf8",
                "unreadable",
                "invalid_path",
                "symlink"
            ]
        );
    }
}
