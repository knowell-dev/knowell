//! Real git repositories in temporary directories, created with the git CLI
//! under fully isolated, deterministic settings: no system or global
//! configuration (so signing, hooks, autocrlf or templates of the machine
//! cannot interfere), fixed identities and dates, no auto-gc.

#![allow(dead_code)]

use std::path::{Path, PathBuf};
use std::process::Command;

use knowell_source::git::GitRepo;

/// Fixed author / committer date: commit ids are reproducible.
pub(crate) const DATE: &str = "2026-01-01T00:00:00+00:00";

/// A temporary directory with an empty global git config file.
pub(crate) struct Sandbox {
    pub(crate) dir: tempfile::TempDir,
    global_config: PathBuf,
}

impl Sandbox {
    pub(crate) fn new() -> Self {
        let dir = tempfile::tempdir().expect("temp dir");
        let global_config = dir.path().join("empty-global.gitconfig");
        std::fs::write(&global_config, "").expect("global config");
        Self { dir, global_config }
    }

    pub(crate) fn path(&self) -> &Path {
        self.dir.path()
    }

    /// A git command running in `cwd` with the isolated environment.
    pub(crate) fn command(&self, cwd: &Path) -> Command {
        let mut cmd = Command::new("git");
        cmd.current_dir(cwd);
        for var in [
            "GIT_DIR",
            "GIT_WORK_TREE",
            "GIT_INDEX_FILE",
            "GIT_OBJECT_DIRECTORY",
            "GIT_ALTERNATE_OBJECT_DIRECTORIES",
            "GIT_COMMON_DIR",
            "GIT_NAMESPACE",
            "GIT_CONFIG",
            "GIT_CONFIG_PARAMETERS",
            "GIT_CONFIG_COUNT",
        ] {
            cmd.env_remove(var);
        }
        cmd.env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_CONFIG_GLOBAL", &self.global_config)
            .env("GIT_AUTHOR_NAME", "Knowell Test")
            .env("GIT_AUTHOR_EMAIL", "test@example.invalid")
            .env("GIT_COMMITTER_NAME", "Knowell Test")
            .env("GIT_COMMITTER_EMAIL", "test@example.invalid")
            .env("GIT_AUTHOR_DATE", DATE)
            .env("GIT_COMMITTER_DATE", DATE)
            .env("GIT_TERMINAL_PROMPT", "0")
            .env("GIT_OPTIONAL_LOCKS", "0")
            .args([
                "-c",
                "commit.gpgsign=false",
                "-c",
                "tag.gpgsign=false",
                "-c",
                "core.autocrlf=false",
                "-c",
                "init.defaultBranch=main",
                "-c",
                "gc.auto=0",
                "-c",
                "maintenance.auto=false",
                "-c",
                "advice.detachedHead=false",
            ]);
        cmd
    }

    /// Runs git in `cwd`, panicking with stderr on failure; returns trimmed
    /// stdout.
    pub(crate) fn git(&self, cwd: &Path, args: &[&str]) -> String {
        let output = self
            .command(cwd)
            .args(args)
            .output()
            .expect("git must be installed to run these tests");
        assert!(
            output.status.success(),
            "git {args:?} failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8(output.stdout)
            .expect("utf-8 git output")
            .trim()
            .to_owned()
    }

    /// Runs git with `input` on stdin; returns trimmed stdout.
    pub(crate) fn git_stdin(&self, cwd: &Path, args: &[&str], input: &[u8]) -> String {
        use std::io::Write;
        use std::process::Stdio;
        let mut child = self
            .command(cwd)
            .args(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("git must be installed to run these tests");
        child
            .stdin
            .take()
            .expect("stdin")
            .write_all(input)
            .expect("write stdin");
        let output = child.wait_with_output().expect("git output");
        assert!(
            output.status.success(),
            "git {args:?} failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8(output.stdout)
            .expect("utf-8 git output")
            .trim()
            .to_owned()
    }

    /// Creates a repository at `<sandbox>/<name>` with repository-local
    /// settings that also pin what `gix` reads (it ignores the CLI `-c`).
    pub(crate) fn init(&self, name: &str) -> PathBuf {
        let path = self.path().join(name);
        std::fs::create_dir_all(&path).expect("repo dir");
        self.git(&path, &["init", "-q"]);
        self.configure(&path);
        path
    }

    /// Repository-local settings, so that `GitRepo::open` (which honours
    /// the user's global configuration) behaves deterministically too.
    pub(crate) fn configure(&self, repo: &Path) {
        let excludes = self.path().join("no-global-excludes");
        std::fs::write(&excludes, "").expect("excludes file");
        for (key, value) in [
            ("core.autocrlf", "false"),
            ("core.safecrlf", "false"),
            (
                "core.excludesFile",
                excludes.to_str().expect("utf-8 temp path"),
            ),
            ("gc.auto", "0"),
            ("commit.gpgsign", "false"),
            ("tag.gpgsign", "false"),
            ("diff.renames", "false"),
        ] {
            self.git(repo, &["config", key, value]);
        }
    }

    /// Writes `rel` (with `/` separators) below `root`, creating parents.
    pub(crate) fn write(&self, root: &Path, rel: &str, bytes: &[u8]) {
        let path = root.join(rel);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).expect("parent dir");
        }
        std::fs::write(path, bytes).expect("write file");
    }

    /// Stages everything (including ignored files when `force`) and
    /// commits; returns the new commit id.
    pub(crate) fn commit_all(&self, repo: &Path, message: &str) -> String {
        self.git(repo, &["add", "-A"]);
        self.git(repo, &["commit", "-q", "--allow-empty", "-m", message]);
        self.git(repo, &["rev-parse", "HEAD"])
    }
}

/// Opens a repository with only its own configuration, so results never
/// depend on the machine running the tests.
pub(crate) fn open(path: &Path) -> GitRepo {
    GitRepo::open_isolated(path).expect("open repository")
}

/// A fake GitHub token, assembled at runtime so no secret-shaped literal
/// exists in the source.
pub(crate) fn fake_token() -> String {
    format!("ghp_{}", "FAKE".repeat(9))
}

/// A canary value that must never appear in any output.
pub(crate) fn canary() -> String {
    format!("KNOWELL_CANARY_{}", "5e1f0d")
}

/// Canonical form of a path, for comparing paths git reports with paths the
/// test created (temporary directories may be reached through links).
pub(crate) fn canon(path: &Path) -> PathBuf {
    std::fs::canonicalize(path).expect("canonicalize")
}
