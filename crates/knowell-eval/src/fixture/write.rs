//! Materialising a fixture on disk, optionally as one git repository per
//! project with a single, fully deterministic commit.

use std::io::ErrorKind;
use std::path::Path;
use std::process::Command;

use knowell_core::{ContentHash, Name, RepoPath};
use serde::Serialize;

use super::{FileRole, Fixture, FixtureInfo, PlantedIssue, planted_issues};
use crate::error::EvalError;

/// Options for [`Fixture::write_to`].
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct WriteOptions {
    /// Turn every project directory into a git repository with one commit on
    /// `main`. The commit id is identical on every machine: author,
    /// committer and dates are fixed, the object format is SHA-1, and the
    /// user's system/global git configuration, attributes, ignore rules,
    /// templates and hooks are bypassed.
    pub git: bool,
}

/// What was written: per-file content hashes and per-project commit ids.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct FixtureManifest {
    /// Fixture identity.
    pub fixture: FixtureInfo,
    /// Whether projects were committed to git.
    pub git: bool,
    /// Name of the workspace configuration file at the root.
    pub workspace_config: String,
    /// Projects sorted by name.
    pub projects: Vec<ManifestProject>,
    /// The planted defects (ground truth for future checks).
    pub planted_issues: Vec<PlantedIssue>,
}

/// One project in a [`FixtureManifest`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ManifestProject {
    /// Project name.
    pub name: Name,
    /// Directory relative to the output root (equals the name).
    pub path: String,
    /// Primary language.
    pub language: String,
    /// Commit id of `main` (40 hex digits) when written with git.
    pub commit: Option<String>,
    /// Files sorted by path.
    pub files: Vec<ManifestFile>,
}

/// One file in a [`FixtureManifest`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ManifestFile {
    /// Path relative to the project root.
    pub path: RepoPath,
    /// Core or noise.
    pub role: FileRole,
    /// Size in bytes.
    pub bytes: u64,
    /// BLAKE3 of the content.
    pub hash: ContentHash,
}

impl FixtureManifest {
    /// Pretty JSON with a trailing newline (field order is fixed).
    pub fn to_json(&self) -> Result<String, EvalError> {
        serde_json::to_string_pretty(self)
            .map(|json| json + "\n")
            .map_err(|e| EvalError::Serialize {
                what: "fixture manifest",
                message: e.to_string(),
            })
    }
}

/// Name of the workspace configuration written at the output root.
const WORKSPACE_CONFIG: &str = "knowell.toml";

const GIT_NAME: &str = "Knowell Fixtures";
const GIT_EMAIL: &str = "fixtures@knowell.invalid";
/// 2026-01-01T00:00:00Z in git's raw date format.
const GIT_DATE: &str = "@1767225600 +0000";

#[cfg(windows)]
const NULL_DEVICE: &str = "NUL";
#[cfg(not(windows))]
const NULL_DEVICE: &str = "/dev/null";

/// Environment variables that would redirect or alter git if inherited
/// (for example when the caller itself runs inside a git hook).
const SCRUBBED_GIT_ENV: &[&str] = &[
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
    "GIT_TEMPLATE_DIR",
    "GIT_DEFAULT_HASH",
    "GIT_REPLACE_REF_BASE",
    "GIT_GRAFT_FILE",
    "GIT_SHALLOW_FILE",
    "GIT_ATTR_SOURCE",
];

pub(super) fn write_fixture(
    fixture: &Fixture,
    dir: &Path,
    options: &WriteOptions,
) -> Result<FixtureManifest, EvalError> {
    if options.git {
        // Fail before touching the filesystem.
        run_git(None, "-", &["--version"])?;
    }
    prepare_output_dir(dir)?;

    let mut projects = Vec::with_capacity(fixture.projects().len());
    for project in fixture.projects() {
        let root = dir.join(project.name.as_str());
        let mut files = Vec::with_capacity(project.files.len());
        for file in &project.files {
            let target = file
                .path
                .components()
                .fold(root.clone(), |acc, part| acc.join(part));
            if let Some(parent) = target.parent() {
                std::fs::create_dir_all(parent).map_err(|source| EvalError::Io {
                    action: "create directory",
                    path: parent.to_path_buf(),
                    source,
                })?;
            }
            std::fs::write(&target, file.content.as_bytes()).map_err(|source| EvalError::Io {
                action: "write",
                path: target.clone(),
                source,
            })?;
            files.push(ManifestFile {
                path: file.path.clone(),
                role: file.role,
                bytes: file.content.len() as u64,
                hash: ContentHash::of(file.content.as_bytes()),
            });
        }
        let commit = if options.git {
            Some(commit_project(
                &root,
                project.name.as_str(),
                project.files.len(),
            )?)
        } else {
            None
        };
        projects.push(ManifestProject {
            name: project.name.clone(),
            path: project.name.as_str().to_owned(),
            language: project.language.clone(),
            commit,
            files,
        });
    }

    let config_path = dir.join(WORKSPACE_CONFIG);
    std::fs::write(&config_path, fixture.workspace_toml()).map_err(|source| EvalError::Io {
        action: "write",
        path: config_path.clone(),
        source,
    })?;

    Ok(FixtureManifest {
        fixture: fixture.info(),
        git: options.git,
        workspace_config: WORKSPACE_CONFIG.to_owned(),
        projects,
        planted_issues: planted_issues().to_vec(),
    })
}

/// Creates `dir` or accepts it when it is an existing, empty directory.
/// Writing into a populated directory could mix stale files into the
/// workspace, so it is refused.
fn prepare_output_dir(dir: &Path) -> Result<(), EvalError> {
    match std::fs::read_dir(dir) {
        Ok(mut entries) => {
            if entries.next().is_some() {
                return Err(EvalError::OutputNotEmpty(dir.to_path_buf()));
            }
            Ok(())
        }
        Err(error) if error.kind() == ErrorKind::NotFound => {
            std::fs::create_dir_all(dir).map_err(|source| EvalError::Io {
                action: "create directory",
                path: dir.to_path_buf(),
                source,
            })
        }
        Err(source) => Err(EvalError::Io {
            action: "read directory",
            path: dir.to_path_buf(),
            source,
        }),
    }
}

fn commit_project(root: &Path, project: &str, expected_files: usize) -> Result<String, EvalError> {
    run_git(
        Some(root),
        project,
        &[
            "init",
            "--quiet",
            "--template=",
            "--object-format=sha1",
            "--initial-branch=main",
        ],
    )?;
    // --force: a user's ignore rules must not drop files (e.g. `.env`).
    run_git(Some(root), project, &["add", "--all", "--force", "--", "."])?;
    let message = format!("Import {project} ({} fixture)", super::FIXTURE_NAME);
    run_git(
        Some(root),
        project,
        &[
            "commit",
            "--quiet",
            "--no-verify",
            "--no-gpg-sign",
            "-m",
            &message,
        ],
    )?;
    let listed = run_git(Some(root), project, &["ls-files", "-z"])?;
    let committed = listed.split('\0').filter(|s| !s.is_empty()).count();
    if committed != expected_files {
        return Err(EvalError::Git {
            project: project.to_owned(),
            args: "ls-files -z".to_owned(),
            detail: format!("committed {committed} files, expected {expected_files}"),
        });
    }
    let head = run_git(Some(root), project, &["rev-parse", "HEAD"])?;
    Ok(head.trim().to_owned())
}

/// Runs git with every user- and system-level influence removed. `dir` is
/// `None` only for the availability probe.
fn run_git(dir: Option<&Path>, project: &str, args: &[&str]) -> Result<String, EvalError> {
    let mut command = Command::new("git");
    if let Some(dir) = dir {
        command.current_dir(dir);
    }
    for name in SCRUBBED_GIT_ENV {
        command.env_remove(name);
    }
    command
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", NULL_DEVICE)
        .env("GIT_ATTR_NOSYSTEM", "1")
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_AUTHOR_NAME", GIT_NAME)
        .env("GIT_AUTHOR_EMAIL", GIT_EMAIL)
        .env("GIT_AUTHOR_DATE", GIT_DATE)
        .env("GIT_COMMITTER_NAME", GIT_NAME)
        .env("GIT_COMMITTER_EMAIL", GIT_EMAIL)
        .env("GIT_COMMITTER_DATE", GIT_DATE);
    let overrides = [
        format!("user.name={GIT_NAME}"),
        format!("user.email={GIT_EMAIL}"),
        "init.defaultBranch=main".to_owned(),
        "commit.gpgSign=false".to_owned(),
        "tag.gpgSign=false".to_owned(),
        "core.autocrlf=false".to_owned(),
        "core.safecrlf=false".to_owned(),
        "core.fileMode=false".to_owned(),
        "core.fsmonitor=false".to_owned(),
        format!("core.attributesFile={NULL_DEVICE}"),
        format!("core.excludesFile={NULL_DEVICE}"),
        "gc.auto=0".to_owned(),
        "maintenance.auto=false".to_owned(),
    ];
    for item in &overrides {
        command.arg("-c").arg(item);
    }
    command.args(args);

    let output = command.output().map_err(|source| {
        if source.kind() == ErrorKind::NotFound {
            EvalError::GitNotFound
        } else {
            EvalError::Io {
                action: "run git in",
                path: dir.map(Path::to_path_buf).unwrap_or_default(),
                source,
            }
        }
    })?;
    if !output.status.success() {
        return Err(EvalError::Git {
            project: project.to_owned(),
            args: args.join(" "),
            detail: format!(
                "{}: {}",
                output.status,
                String::from_utf8_lossy(&output.stderr).trim()
            ),
        });
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

#[cfg(test)]
mod tests {
    use super::super::{FixtureSpec, Scale, generate};
    use super::*;

    fn small() -> Fixture {
        generate(&FixtureSpec {
            seed: 42,
            scale: Scale::Small,
        })
    }

    #[test]
    fn writes_every_file_and_the_workspace_config() {
        let tmp = tempfile::tempdir().unwrap();
        let out = tmp.path().join("ws");
        let fixture = small();
        let manifest = fixture.write_to(&out, &WriteOptions::default()).unwrap();
        assert!(!manifest.git);
        assert_eq!(manifest.fixture, fixture.info());
        assert_eq!(
            std::fs::read_to_string(out.join("knowell.toml")).unwrap(),
            fixture.workspace_toml()
        );
        let mut count = 0;
        for project in &manifest.projects {
            assert!(project.commit.is_none());
            for file in &project.files {
                let disk = out.join(&project.path).join(file.path.as_str());
                let bytes = std::fs::read(&disk).unwrap();
                assert_eq!(ContentHash::of(&bytes), file.hash, "{}", disk.display());
                assert_eq!(bytes.len() as u64, file.bytes);
                count += 1;
            }
        }
        assert_eq!(count, fixture.file_count());
        let json = manifest.to_json().unwrap();
        assert!(json.ends_with('\n'));
        for canary in fixture.canaries() {
            assert!(
                !json.contains(&canary),
                "manifest must not carry secret values"
            );
        }
    }

    #[test]
    fn refuses_a_non_empty_directory() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("stale.txt"), "x").unwrap();
        let err = small()
            .write_to(tmp.path(), &WriteOptions::default())
            .unwrap_err();
        assert!(matches!(err, EvalError::OutputNotEmpty(_)), "{err}");
    }

    #[test]
    fn accepts_an_existing_empty_directory() {
        let tmp = tempfile::tempdir().unwrap();
        assert!(
            small()
                .write_to(tmp.path(), &WriteOptions::default())
                .is_ok()
        );
    }
}
