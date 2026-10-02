//! `$KNOWELL_HOME/workspaces.json`: which workspace file each registered
//! workspace was read from.
//!
//! The database keeps the hierarchy (organization, workspace, projects,
//! sources, views) but not where the `knowell.toml` lives; `know serve` uses
//! this file to show effective settings and where they come from.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, bail};
use knowell_core::Name;
use serde::{Deserialize, Serialize};

use crate::fsutil;

const FILE_NAME: &str = "workspaces.json";
const VERSION: u32 = 1;

/// One registered workspace.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Entry {
    pub(crate) organization: Name,
    pub(crate) workspace: Name,
    pub(crate) file: PathBuf,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct RegistryFile {
    version: u32,
    #[serde(default)]
    workspaces: Vec<Entry>,
}

/// The registry, loaded from `home`.
#[derive(Debug, Default)]
pub(crate) struct Registry {
    entries: Vec<Entry>,
}

impl Registry {
    pub(crate) fn path(home: &Path) -> PathBuf {
        home.join(FILE_NAME)
    }

    /// Loads the registry; an absent file is an empty registry.
    pub(crate) fn load(home: &Path) -> anyhow::Result<Self> {
        let path = Self::path(home);
        let Some(text) = fsutil::read_optional(&path)? else {
            return Ok(Self::default());
        };
        let file: RegistryFile = serde_json::from_str(&text)
            .map_err(|e| anyhow::anyhow!("line {}, column {}", e.line(), e.column()))
            .with_context(|| format!("{} is not a valid workspace registry", path.display()))?;
        if file.version != VERSION {
            bail!(
                "{} has version {}; this build understands version {VERSION}",
                path.display(),
                file.version
            );
        }
        Ok(Self {
            entries: file.workspaces,
        })
    }

    /// Adds or replaces the entry of (organization, workspace).
    pub(crate) fn upsert(&mut self, entry: Entry) {
        self.entries
            .retain(|e| !(e.organization == entry.organization && e.workspace == entry.workspace));
        self.entries.push(entry);
        self.entries.sort_by(|a, b| {
            (a.organization.as_str(), a.workspace.as_str())
                .cmp(&(b.organization.as_str(), b.workspace.as_str()))
        });
    }

    /// The workspace file of (organization, workspace), if registered.
    pub(crate) fn file_of(&self, organization: &Name, workspace: &Name) -> Option<&Path> {
        self.entries
            .iter()
            .find(|e| &e.organization == organization && &e.workspace == workspace)
            .map(|e| e.file.as_path())
    }

    /// Workspace files of one organization, by workspace name.
    pub(crate) fn files_of(&self, organization: &Name) -> BTreeMap<Name, PathBuf> {
        self.entries
            .iter()
            .filter(|e| &e.organization == organization)
            .map(|e| (e.workspace.clone(), e.file.clone()))
            .collect()
    }

    pub(crate) fn save(&self, home: &Path) -> anyhow::Result<()> {
        let file = RegistryFile {
            version: VERSION,
            workspaces: self.entries.clone(),
        };
        let mut text = serde_json::to_string_pretty(&file)?;
        text.push('\n');
        fsutil::write_atomic(&Self::path(home), &text)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn n(s: &str) -> Name {
        Name::new(s).unwrap()
    }

    #[test]
    fn upsert_replaces_and_round_trips() {
        let dir = tempfile::tempdir().unwrap();
        let mut reg = Registry::load(dir.path()).unwrap();
        reg.upsert(Entry {
            organization: n("local"),
            workspace: n("b"),
            file: "/x/b.toml".into(),
        });
        reg.upsert(Entry {
            organization: n("local"),
            workspace: n("a"),
            file: "/x/a.toml".into(),
        });
        reg.upsert(Entry {
            organization: n("local"),
            workspace: n("b"),
            file: "/y/b.toml".into(),
        });
        reg.save(dir.path()).unwrap();
        let back = Registry::load(dir.path()).unwrap();
        let files = back.files_of(&n("local"));
        assert_eq!(files.len(), 2);
        assert_eq!(
            back.file_of(&n("local"), &n("b")),
            Some(Path::new("/y/b.toml"))
        );
        assert!(back.files_of(&n("other")).is_empty());
    }

    #[test]
    fn malformed_registry_is_reported() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(Registry::path(dir.path()), "{not json").unwrap();
        assert!(Registry::load(dir.path()).is_err());
        std::fs::write(Registry::path(dir.path()), r#"{"version":9}"#).unwrap();
        assert!(Registry::load(dir.path()).is_err());
    }
}
