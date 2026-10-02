//! `know project add`: appends a `[[project]]` table to knowell.toml.
//!
//! The file is edited as text (comments and layout stay), the result is
//! validated and resolved with `knowell-config` before it replaces the file,
//! and nothing is written when any check fails.

use std::path::{Path, PathBuf};
use std::process::ExitCode;

use anyhow::Context;
use clap::Subcommand;
use knowell_core::{Name, RepoPath, TrackTarget};

use crate::env::{self, Env};
use crate::fsutil;
use crate::output::Output;

#[derive(Debug, Subcommand)]
pub(crate) enum ProjectCommand {
    /// Add a project to the workspace file.
    Add {
        /// Directory of the project's repository (or folder).
        path: PathBuf,
        /// Project name [default: derived from the directory (and --root)].
        #[arg(long)]
        name: Option<Name>,
        /// Ref to follow, e.g. `branch:main` [default: the workspace's `track`].
        #[arg(long)]
        track: Option<TrackTarget>,
        /// Sub-directory of the repository for a monorepo sub-project
        /// (`/`-separated, relative to PATH).
        #[arg(long)]
        root: Option<RepoPath>,
    },
}

pub(crate) fn run(cmd: ProjectCommand, env: &Env, out: &mut Output) -> anyhow::Result<ExitCode> {
    let ProjectCommand::Add {
        path,
        name,
        track,
        root,
    } = cmd;
    let file = env.require_workspace()?;
    match add(&file, &path, name, track, root)? {
        Ok(added) => {
            out.line(format!(
                "added project `{}` ({}{}) to {}",
                added.name,
                added.path,
                added
                    .root
                    .as_ref()
                    .map(|r| format!(", root {r}"))
                    .unwrap_or_default(),
                file.display()
            ))?;
            out.line("next: `know workspace add` registers the change with the engine")?;
            out.flush()?;
            Ok(ExitCode::SUCCESS)
        }
        Err(refusal) => {
            tracing::error!("{refusal}; {} was not changed", file.display());
            Ok(ExitCode::FAILURE)
        }
    }
}

/// What was appended.
#[derive(Debug)]
pub(crate) struct Added {
    pub(crate) name: Name,
    pub(crate) path: String,
    pub(crate) root: Option<RepoPath>,
}

/// Appends the project. The inner `Err` is a refusal (exit 1) with the
/// reason; the outer one an operational failure.
fn add(
    file: &Path,
    project_dir: &Path,
    name: Option<Name>,
    track: Option<TrackTarget>,
    root: Option<RepoPath>,
) -> anyhow::Result<Result<Added, String>> {
    let text =
        std::fs::read_to_string(file).with_context(|| format!("cannot read {}", file.display()))?;
    let config = match knowell_config::parse_workspace(&text) {
        Ok(config) => config,
        Err(err) => {
            return Ok(Err(format!(
                "the workspace file is invalid ({err}); fix it first"
            )));
        }
    };
    let base = env::parent_dir(file)?;
    let abs = env::absolute(project_dir)?;
    if !abs.is_dir() {
        return Ok(Err(format!("{} is not a directory", abs.display())));
    }
    if let Some(root) = &root
        && !abs.join(root.as_str()).is_dir()
    {
        return Ok(Err(format!(
            "root `{root}` is not a directory inside {}",
            abs.display()
        )));
    }
    let rel = relative_to(&abs, &base);
    let name = match name {
        Some(name) => name,
        None => match default_name(&abs, root.as_ref()) {
            Some(name) => name,
            None => {
                return Ok(Err(
                    "cannot derive a project name from the directory; pass --name".to_owned(),
                ));
            }
        },
    };
    if config.project.iter().any(|p| p.name == name) {
        return Ok(Err(format!(
            "a project named `{name}` already exists; pass another --name"
        )));
    }
    let same_place = config.project.iter().find(|p| {
        p.root == root && env::absolute(&base.join(&p.path)).is_ok_and(|existing| existing == abs)
    });
    if let Some(existing) = same_place {
        return Ok(Err(format!(
            "project `{}` already covers this directory{}",
            existing.name,
            root.as_ref()
                .map(|r| format!(" and root `{r}`"))
                .unwrap_or_default()
        )));
    }

    let block = project_block(&name, &rel, root.as_ref(), track.as_ref());
    let mut new_text = text.clone();
    if !new_text.is_empty() && !new_text.ends_with('\n') {
        new_text.push('\n');
    }
    new_text.push_str(&block);

    let new_config = match knowell_config::parse_workspace(&new_text) {
        Ok(config) => config,
        Err(err) => return Ok(Err(format!("the result would be invalid ({err})"))),
    };
    if let Err(issues) = new_config.resolve(&base) {
        let hint = if track.is_none() && config.workspace.track.is_none() {
            " (the workspace sets no `track`; pass --track, e.g. --track branch:main)"
        } else {
            ""
        };
        return Ok(Err(format!("the result does not resolve: {issues}{hint}")));
    }
    fsutil::write_atomic(file, &new_text)?;
    // Read back what is on disk now, so a concurrent edit cannot slip through unseen.
    knowell_config::load_workspace(file)
        .map_err(|e| anyhow::anyhow!("{} became invalid after writing: {e}", file.display()))?;
    Ok(Ok(Added {
        name,
        path: rel,
        root,
    }))
}

/// `[[project]]` table text; values are quoted by `toml`.
fn project_block(
    name: &Name,
    path: &str,
    root: Option<&RepoPath>,
    track: Option<&TrackTarget>,
) -> String {
    let q = |s: &str| toml::Value::String(s.to_owned()).to_string();
    let mut block = format!(
        "\n# added by `know project add`\n[[project]]\nname = {}\npath = {}\n",
        q(name.as_str()),
        q(path)
    );
    if let Some(root) = root {
        block.push_str(&format!("root = {}\n", q(root.as_str())));
    }
    if let Some(track) = track {
        block.push_str(&format!("track = {}\n", q(&track.to_string())));
    }
    block
}

/// `path` relative to `base` with `/` separators when it lies inside it
/// (`.` for `base` itself), else the absolute path.
fn relative_to(path: &Path, base: &Path) -> String {
    match path.strip_prefix(base) {
        Ok(rel) if rel.as_os_str().is_empty() => ".".to_owned(),
        Ok(rel) => fsutil::slash_path(rel),
        Err(_) => fsutil::slash_path(path),
    }
}

/// A valid name from the directory (plus the root's last component).
fn default_name(dir: &Path, root: Option<&RepoPath>) -> Option<Name> {
    let base = dir.file_name()?.to_string_lossy().into_owned();
    let text = match root {
        Some(root) => format!("{base}-{}", root.file_name()),
        None => base,
    };
    Name::new(slugify(&text)).ok()
}

/// Lowercase ASCII letters and digits joined by single `-`.
fn slugify(text: &str) -> String {
    let mut slug = String::new();
    for c in text.chars() {
        if c.is_ascii_alphanumeric() {
            slug.push(c.to_ascii_lowercase());
        } else if !slug.is_empty() && !slug.ends_with('-') {
            slug.push('-');
        }
    }
    while slug.ends_with('-') {
        slug.pop();
    }
    slug
}

#[cfg(test)]
mod tests {
    use super::*;

    fn workspace(dir: &Path, extra: &str) -> PathBuf {
        let file = dir.join("knowell.toml");
        std::fs::write(
            &file,
            format!(
                "version = 1\n# keep this comment\n[workspace]\nname = \"w\"\ntrack = \"branch:main\"\n{extra}"
            ),
        )
        .unwrap();
        file
    }

    #[test]
    fn slugs() {
        assert_eq!(slugify("My_Service.v2"), "my-service-v2");
        assert_eq!(slugify("--x--"), "x");
        assert_eq!(slugify("..."), "");
    }

    #[test]
    fn appends_and_keeps_comments() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("Api_Server/sub")).unwrap();
        let file = workspace(dir.path(), "");
        let added = add(
            &file,
            &dir.path().join("Api_Server"),
            None,
            None,
            Some(RepoPath::new("sub").unwrap()),
        )
        .unwrap()
        .unwrap();
        assert_eq!(added.name.as_str(), "api-server-sub");
        assert_eq!(added.path, "Api_Server");
        let text = std::fs::read_to_string(&file).unwrap();
        assert!(text.contains("# keep this comment"));
        assert!(text.contains("root = \"sub\""));
        let config = knowell_config::load_workspace(&file).unwrap();
        assert_eq!(config.project.len(), 1);
    }

    #[test]
    fn refuses_duplicates_and_missing_tracks() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("a")).unwrap();
        let file = workspace(dir.path(), "");
        add(&file, &dir.path().join("a"), None, None, None)
            .unwrap()
            .unwrap();
        let before = std::fs::read_to_string(&file).unwrap();
        let dup = add(&file, &dir.path().join("a"), None, None, None).unwrap();
        assert!(dup.unwrap_err().contains("already exists"));
        let same_place = add(
            &file,
            &dir.path().join("a"),
            Some(Name::new("other").unwrap()),
            None,
            None,
        )
        .unwrap();
        assert!(same_place.unwrap_err().contains("already covers"));
        assert_eq!(std::fs::read_to_string(&file).unwrap(), before);

        let untracked = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(untracked.path().join("b")).unwrap();
        let file = untracked.path().join("knowell.toml");
        std::fs::write(&file, "version = 1\n[workspace]\nname = \"w\"\n").unwrap();
        let refused = add(&file, &untracked.path().join("b"), None, None, None).unwrap();
        assert!(refused.unwrap_err().contains("--track"));
        let ok = add(
            &file,
            &untracked.path().join("b"),
            None,
            Some("tag:v1.0.0".parse().unwrap()),
            None,
        )
        .unwrap();
        assert!(ok.is_ok());
    }

    #[test]
    fn paths_outside_the_workspace_stay_absolute() {
        let base = Path::new("/ws");
        assert_eq!(relative_to(Path::new("/ws"), base), ".");
        assert_eq!(relative_to(Path::new("/ws/a/b"), base), "a/b");
        assert!(relative_to(Path::new("/elsewhere/x"), base).ends_with("elsewhere/x"));
    }
}
