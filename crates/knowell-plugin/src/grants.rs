//! Capabilities the user grants to a plugin. Nothing is granted by default.

use std::fmt;
use std::fs::{self, File};
use std::io::{self, Read};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use knowell_core::RepoPath;

use crate::error::PluginError;
use crate::manifest::Capability;

/// Decides whether a plugin may read a project file. Called with the requested
/// path and again with the path it resolves to (after symlinks), before the
/// file is opened; both must be allowed.
pub type PathFilter = dyn Fn(&RepoPath) -> bool + Send + Sync;

/// The capabilities a user granted to one plugin. A plugin is refused at load
/// time if its manifest requests a capability that is not granted here, and
/// it never receives a grant it did not request.
#[derive(Debug, Clone, Default)]
pub struct Grants {
    project_files: Option<ProjectFilesGrant>,
}

impl Grants {
    /// No capabilities.
    pub fn none() -> Self {
        Self::default()
    }

    /// Adds read-only access to one project's files.
    pub fn with_project_files(mut self, grant: ProjectFilesGrant) -> Self {
        self.project_files = Some(grant);
        self
    }

    /// The `project-files` grant, if any.
    pub fn project_files(&self) -> Option<&ProjectFilesGrant> {
        self.project_files.as_ref()
    }

    /// Whether `capability` is granted.
    pub fn grants(&self, capability: Capability) -> bool {
        match capability {
            Capability::ProjectFiles => self.project_files.is_some(),
        }
    }
}

/// Read-only access to UTF-8 files under one project root.
///
/// The host enforces, for every read: the path is a valid [`RepoPath`] (no
/// absolute paths, drive prefixes, `..`, backslashes, `:` alternate data
/// streams, or components ending in `.`/space); after resolving symlinks it is
/// still inside the root; it is a regular file within the size limit; no
/// component is `.git` or `.knowell`; and `filter` allows both the requested
/// and the resolved path. Integrators pass Knowell's sensitive-file policy
/// (for example `knowell-secrets`' exclusion check) as `filter`, so a plugin
/// can never read `.env` files, keys or credentials.
#[derive(Clone)]
pub struct ProjectFilesGrant {
    root: PathBuf,
    filter: Arc<PathFilter>,
}

impl ProjectFilesGrant {
    /// Grants access below `root`, which must be an existing directory. The
    /// root is canonicalised once here.
    pub fn new(
        root: impl AsRef<Path>,
        filter: impl Fn(&RepoPath) -> bool + Send + Sync + 'static,
    ) -> Result<Self, PluginError> {
        let root = root.as_ref();
        let canonical = fs::canonicalize(root).map_err(|e| {
            PluginError::InvalidGrant(format!(
                "project root `{}` cannot be resolved: {e}",
                root.display()
            ))
        })?;
        if !canonical.is_dir() {
            return Err(PluginError::InvalidGrant(format!(
                "project root `{}` is not a directory",
                root.display()
            )));
        }
        Ok(Self {
            root: canonical,
            filter: Arc::new(filter),
        })
    }

    /// The canonical project root.
    pub fn root(&self) -> &Path {
        &self.root
    }

    fn allows(&self, path: &RepoPath) -> bool {
        !path.components().any(is_internal_dir) && (self.filter)(path)
    }

    /// Reads `requested` (a plugin-supplied path) under the policy above.
    pub(crate) fn read(&self, requested: &str, max_bytes: u64) -> Result<String, ReadFailure> {
        if requested.contains(':') || requested.chars().any(char::is_control) {
            return Err(ReadFailure::Denied);
        }
        let path = RepoPath::new(requested).map_err(|_| ReadFailure::Denied)?;
        if path.components().any(has_ignored_suffix) || !self.allows(&path) {
            return Err(ReadFailure::Denied);
        }
        let mut joined = self.root.clone();
        joined.extend(path.components());
        let resolved = fs::canonicalize(&joined).map_err(|e| match e.kind() {
            io::ErrorKind::NotFound | io::ErrorKind::NotADirectory => ReadFailure::NotFound,
            _ => ReadFailure::Unavailable,
        })?;
        // Symlinks (and Windows case or short-name aliases) are judged by
        // where they lead, not by how they were named.
        let relative = resolved
            .strip_prefix(&self.root)
            .map_err(|_| ReadFailure::Denied)?;
        let resolved_path = RepoPath::from_relative(relative).map_err(|_| ReadFailure::Denied)?;
        if !self.allows(&resolved_path) {
            return Err(ReadFailure::Denied);
        }
        let metadata = fs::metadata(&resolved).map_err(|_| ReadFailure::Unavailable)?;
        if !metadata.is_file() {
            return Err(ReadFailure::NotFound);
        }
        if metadata.len() > max_bytes {
            return Err(ReadFailure::TooLarge);
        }
        let file = File::open(&resolved).map_err(|_| ReadFailure::Unavailable)?;
        // The file may have grown since `metadata`; never read past the limit.
        let mut bytes = Vec::new();
        file.take(max_bytes.saturating_add(1))
            .read_to_end(&mut bytes)
            .map_err(|_| ReadFailure::Unavailable)?;
        if u64::try_from(bytes.len()).unwrap_or(u64::MAX) > max_bytes {
            return Err(ReadFailure::TooLarge);
        }
        String::from_utf8(bytes).map_err(|_| ReadFailure::NotText)
    }
}

impl fmt::Debug for ProjectFilesGrant {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ProjectFilesGrant")
            .field("root", &self.root)
            .finish_non_exhaustive()
    }
}

/// Engine-internal directories a plugin never reads, whatever the filter says.
fn is_internal_dir(component: &str) -> bool {
    component.eq_ignore_ascii_case(".git") || component.eq_ignore_ascii_case(".knowell")
}

/// Windows silently drops trailing dots and spaces, which would let `.env.`
/// slip past a filter that matches `.env`.
fn has_ignored_suffix(component: &str) -> bool {
    component.ends_with('.') || component.ends_with(' ')
}

/// Why a `project-files` read was refused; maps 1:1 to the WIT `read-error`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ReadFailure {
    Denied,
    NotFound,
    TooLarge,
    NotText,
    Unavailable,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn project() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        fs::create_dir_all(root.join("src")).unwrap();
        fs::create_dir_all(root.join(".git")).unwrap();
        fs::write(root.join("src/app.toy"), "route GET /a -> a\n").unwrap();
        fs::write(root.join(".git/config"), "[core]\n").unwrap();
        fs::write(root.join(".env"), "KNOWELL_CANARY_TOKEN=fake\n").unwrap();
        fs::write(root.join("binary.bin"), [0xff, 0xfe, 0x00]).unwrap();
        fs::write(root.join("big.txt"), "x".repeat(100)).unwrap();
        dir
    }

    fn no_env(path: &RepoPath) -> bool {
        !path.file_name().starts_with(".env")
    }

    #[test]
    fn reads_allowed_files() {
        let dir = project();
        let grant = ProjectFilesGrant::new(dir.path(), no_env).unwrap();
        assert_eq!(
            grant.read("src/app.toy", 1024).unwrap(),
            "route GET /a -> a\n"
        );
    }

    #[test]
    fn denies_escapes_and_policy_violations() {
        let dir = project();
        let grant = ProjectFilesGrant::new(dir.path(), no_env).unwrap();
        for path in [
            "../outside.txt",
            "src/../../outside.txt",
            "/etc/passwd",
            "C:/Windows/win.ini",
            "c:x",
            "src\\app.toy",
            "src/./app.toy",
            "src//app.toy",
            "",
            ".env",
            ".env.",
            "src/app.toy:stream",
            "src/app.toy ",
            ".git/config",
            ".GIT/config",
            "src/\u{0}app.toy",
            "src",
        ] {
            let result = grant.read(path, 1024);
            assert!(
                matches!(
                    result,
                    Err(ReadFailure::Denied) | Err(ReadFailure::NotFound)
                ),
                "{path:?} -> {result:?}"
            );
        }
        // These are refused specifically by policy, not by absence.
        for path in [".env", "../x", ".git/config", "/etc/passwd"] {
            assert_eq!(grant.read(path, 1024), Err(ReadFailure::Denied), "{path}");
        }
    }

    #[test]
    fn classifies_other_failures() {
        let dir = project();
        let grant = ProjectFilesGrant::new(dir.path(), |_: &RepoPath| true).unwrap();
        assert_eq!(grant.read("missing.txt", 1024), Err(ReadFailure::NotFound));
        assert_eq!(
            grant.read("src/missing/x.txt", 1024),
            Err(ReadFailure::NotFound)
        );
        assert_eq!(grant.read("src", 1024), Err(ReadFailure::NotFound));
        assert_eq!(grant.read("binary.bin", 1024), Err(ReadFailure::NotText));
        assert_eq!(grant.read("big.txt", 99), Err(ReadFailure::TooLarge));
        assert_eq!(grant.read("big.txt", 100).map(|s| s.len()), Ok(100));
    }

    #[test]
    fn filter_sees_the_resolved_path() {
        let dir = project();
        let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
        let log = Arc::clone(&seen);
        let grant = ProjectFilesGrant::new(dir.path(), move |p: &RepoPath| {
            log.lock().unwrap().push(p.as_str().to_string());
            true
        })
        .unwrap();
        grant.read("src/app.toy", 1024).unwrap();
        assert_eq!(*seen.lock().unwrap(), ["src/app.toy", "src/app.toy"]);
    }

    #[cfg(unix)]
    #[test]
    fn symlinks_are_judged_by_their_target() {
        let dir = project();
        let outside = tempfile::tempdir().unwrap();
        fs::write(outside.path().join("secret.txt"), "outside").unwrap();
        std::os::unix::fs::symlink(
            outside.path().join("secret.txt"),
            dir.path().join("link.txt"),
        )
        .unwrap();
        std::os::unix::fs::symlink(dir.path().join(".env"), dir.path().join("env-link.txt"))
            .unwrap();
        let grant = ProjectFilesGrant::new(dir.path(), no_env).unwrap();
        assert_eq!(grant.read("link.txt", 1024), Err(ReadFailure::Denied));
        assert_eq!(grant.read("env-link.txt", 1024), Err(ReadFailure::Denied));
    }

    #[test]
    fn grant_requires_an_existing_directory() {
        let dir = project();
        assert!(matches!(
            ProjectFilesGrant::new(dir.path().join("missing"), |_: &RepoPath| true),
            Err(PluginError::InvalidGrant(_))
        ));
        assert!(matches!(
            ProjectFilesGrant::new(dir.path().join("big.txt"), |_: &RepoPath| true),
            Err(PluginError::InvalidGrant(_))
        ));
    }

    #[test]
    fn grants_are_explicit() {
        let dir = project();
        assert!(!Grants::none().grants(Capability::ProjectFiles));
        let grants = Grants::none()
            .with_project_files(ProjectFilesGrant::new(dir.path(), |_: &RepoPath| true).unwrap());
        assert!(grants.grants(Capability::ProjectFiles));
        assert!(format!("{grants:?}").contains("ProjectFilesGrant"));
    }
}
