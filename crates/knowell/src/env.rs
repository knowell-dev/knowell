//! Where things live: the Knowell home directory, the engine configuration
//! file, the workspace file and the user's home directory.

use std::path::{Path, PathBuf};

use anyhow::{Context, bail};
use knowell_config::{ConfigError, EngineConfig};

use crate::GlobalArgs;

/// File name of a workspace configuration.
pub(crate) const WORKSPACE_FILE: &str = "knowell.toml";

/// Resolved locations for one invocation.
#[derive(Debug, Clone)]
pub(crate) struct Env {
    /// `$KNOWELL_HOME`, default `~/.knowell`: engine config, managed
    /// PostgreSQL, workspace registry.
    pub(crate) home: PathBuf,
    /// The engine configuration file (`--config` or `$KNOWELL_HOME/config.toml`).
    pub(crate) engine_config: PathBuf,
    /// `--workspace`, if given.
    workspace_flag: Option<PathBuf>,
}

impl Env {
    pub(crate) fn from_globals(global: &GlobalArgs) -> anyhow::Result<Self> {
        let home = knowell_home()?;
        let engine_config = global
            .engine_config
            .clone()
            .unwrap_or_else(|| home.join("config.toml"));
        Ok(Self {
            home,
            engine_config,
            workspace_flag: global.workspace_file.clone(),
        })
    }

    /// The workspace file: `--workspace` if given (it must exist), else
    /// `knowell.toml` in the current directory or the nearest parent.
    pub(crate) fn find_workspace(&self) -> anyhow::Result<Option<PathBuf>> {
        find_workspace(self.workspace_flag.as_deref())
    }

    /// Like [`Env::find_workspace`], but a missing file is an error.
    pub(crate) fn require_workspace(&self) -> anyhow::Result<PathBuf> {
        match self.find_workspace()? {
            Some(path) => Ok(path),
            None => bail!(
                "no {WORKSPACE_FILE} in this directory or above; create one with `know workspace import` or pass --workspace"
            ),
        }
    }

    /// Loads the engine configuration; `Ok(None)` when the file does not exist.
    pub(crate) fn load_engine(&self) -> Result<Option<EngineConfig>, ConfigError> {
        if !self.engine_config.exists() {
            return Ok(None);
        }
        knowell_config::load_engine(&self.engine_config).map(Some)
    }

    /// Loads the engine configuration; a missing file is an error that tells
    /// the user to run `know init`.
    pub(crate) fn require_engine(&self) -> anyhow::Result<EngineConfig> {
        match self.load_engine() {
            Ok(Some(cfg)) => Ok(cfg),
            Ok(None) => bail!(
                "no engine configuration at {}; run `know init` first",
                self.engine_config.display()
            ),
            // ConfigError never quotes configuration values.
            Err(err) => Err(anyhow::Error::new(err)),
        }
    }
}

/// `$KNOWELL_HOME`, or `~/.knowell`.
pub(crate) fn knowell_home() -> anyhow::Result<PathBuf> {
    if let Some(home) = std::env::var_os("KNOWELL_HOME").filter(|v| !v.is_empty()) {
        return absolute(Path::new(&home));
    }
    Ok(user_home()?.join(".knowell"))
}

/// The user's home directory (`USERPROFILE` on Windows, `HOME` elsewhere).
pub(crate) fn user_home() -> anyhow::Result<PathBuf> {
    let names: &[&str] = if cfg!(windows) {
        &["USERPROFILE", "HOME"]
    } else {
        &["HOME"]
    };
    for name in names {
        if let Some(value) = std::env::var_os(name).filter(|v| !v.is_empty()) {
            return Ok(PathBuf::from(value));
        }
    }
    bail!("cannot determine the home directory; set KNOWELL_HOME (and HOME)")
}

/// Searches `knowell.toml` from the current directory upward, unless
/// `explicit` names the file.
pub(crate) fn find_workspace(explicit: Option<&Path>) -> anyhow::Result<Option<PathBuf>> {
    if let Some(path) = explicit {
        if !path.is_file() {
            bail!("workspace file {} does not exist", path.display());
        }
        return absolute(path).map(Some);
    }
    let cwd = std::env::current_dir().context("cannot read the current directory")?;
    let mut dir: Option<&Path> = Some(&cwd);
    while let Some(d) = dir {
        let candidate = d.join(WORKSPACE_FILE);
        if candidate.is_file() {
            return Ok(Some(candidate));
        }
        dir = d.parent();
    }
    Ok(None)
}

/// An absolute path without touching the file system (no symlink
/// resolution, no `\\?\` prefixes on Windows).
pub(crate) fn absolute(path: &Path) -> anyhow::Result<PathBuf> {
    std::path::absolute(path).with_context(|| format!("cannot resolve {}", path.display()))
}

/// The directory holding a file, as an absolute path.
pub(crate) fn parent_dir(file: &Path) -> anyhow::Result<PathBuf> {
    let abs = absolute(file)?;
    match abs.parent() {
        Some(parent) => Ok(parent.to_path_buf()),
        None => bail!("{} has no parent directory", abs.display()),
    }
}
