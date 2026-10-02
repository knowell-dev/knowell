//! Workspace configuration (`knowell.toml`): which projects belong together
//! and how they are tracked, embedded and filtered.

use std::collections::HashMap;
use std::path::{Component, Path, PathBuf};

use knowell_core::{Name, RepoPath, TrackTarget};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::engine::SUPPORTED_VERSION;
use crate::error::ConfigIssue;

/// Smallest vector size Knowell accepts.
pub const MIN_DIMENSIONS: u32 = 32;
/// Largest vector size Knowell accepts (pgvector `halfvec` HNSW limit).
pub const MAX_DIMENSIONS: u32 = 4000;

/// A workspace: a named group of projects indexed and searched together.
///
/// Stored as `knowell.toml` next to (or above) the projects. Settings in
/// `[workspace]` are defaults that each `[[project]]` may override.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct WorkspaceConfig {
    /// Configuration format version. Must be `1`.
    #[schemars(range(min = 1, max = 1))]
    pub version: u32,
    /// Workspace-wide settings and defaults.
    pub workspace: WorkspaceSection,
    /// The projects of this workspace (`[[project]]` tables).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub project: Vec<ProjectConfig>,
}

/// Whether content may leave the machine.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "kebab-case")]
pub enum DataPolicy {
    /// Nothing leaves this machine: only local embedding providers may be used.
    #[default]
    LocalOnly,
    /// Content may be sent to the configured cloud embedding provider.
    Cloud,
}

impl DataPolicy {
    /// Name as written in the configuration file.
    pub fn as_str(self) -> &'static str {
        match self {
            DataPolicy::LocalOnly => "local-only",
            DataPolicy::Cloud => "cloud",
        }
    }
}

/// Embedding vector size presets.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "kebab-case")]
pub enum EmbeddingPreset {
    /// 768 dimensions: smallest index, fastest search.
    Compact,
    /// 1536 dimensions: good quality/size balance (default).
    #[default]
    Balanced,
    /// 3072 dimensions: best recall, largest index.
    Extended,
    /// Any size from 32 to 4000; requires `dimensions`.
    Custom,
}

impl EmbeddingPreset {
    /// Vector size of the preset; `None` for `custom`.
    pub fn dimensions(self) -> Option<u32> {
        match self {
            EmbeddingPreset::Compact => Some(768),
            EmbeddingPreset::Balanced => Some(1536),
            EmbeddingPreset::Extended => Some(3072),
            EmbeddingPreset::Custom => None,
        }
    }

    /// Name as written in the configuration file.
    pub fn as_str(self) -> &'static str {
        match self {
            EmbeddingPreset::Compact => "compact",
            EmbeddingPreset::Balanced => "balanced",
            EmbeddingPreset::Extended => "extended",
            EmbeddingPreset::Custom => "custom",
        }
    }
}

/// Embedding settings. All fields are optional; a project's fields override
/// the workspace's. `preset` and `dimensions` travel together: a project that
/// sets either one replaces both workspace values.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct EmbeddingConfig {
    /// Name of a provider defined in the engine config (`[providers.<name>]`).
    /// Without any provider, only lexical and graph search are available.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider: Option<Name>,
    /// Embedding model. Falls back to the provider's `model`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// Vector size preset. Defaults to `balanced` (1536).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub preset: Option<EmbeddingPreset>,
    /// Vector size, 32 to 4000. Required when `preset = "custom"`; otherwise
    /// optional but must equal the preset's size. Requires `preset`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(range(min = 32, max = 4000))]
    pub dimensions: Option<u32>,
}

/// The `[workspace]` table.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct WorkspaceSection {
    /// Workspace name (lowercase slug).
    pub name: Name,
    /// Free-text description.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// Default ref for projects that do not set `track`. Knowell never
    /// assumes a default branch: every project needs a target from here or
    /// from itself.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub track: Option<TrackTarget>,
    /// Default data policy for projects. Built-in default: `local-only`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data_policy: Option<DataPolicy>,
    /// Glob patterns (gitignore style) excluded from indexing in every
    /// project. Projects add their own on top.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub exclude: Vec<String>,
    /// Default embedding settings.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub embedding: Option<EmbeddingConfig>,
}

/// One `[[project]]` table.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ProjectConfig {
    /// Project name (lowercase slug), unique within the workspace.
    pub name: Name,
    /// Directory of the project, relative to the directory holding this file,
    /// or absolute.
    pub path: PathBuf,
    /// Sub-directory (relative to `path`, `/`-separated) for monorepo
    /// projects that cover only part of a repository.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub root: Option<RepoPath>,
    /// Clone URL of the repository. Informational only; never contacted
    /// without an explicit fetch request.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub remote: Option<String>,
    /// Ref to follow, overriding `workspace.track`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub track: Option<TrackTarget>,
    /// Data policy, overriding `workspace.data_policy`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data_policy: Option<DataPolicy>,
    /// Extra exclude globs, appended to the workspace's.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub exclude: Vec<String>,
    /// Embedding overrides for this project.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub embedding: Option<EmbeddingConfig>,
}

impl EmbeddingConfig {
    /// Validates one layer in isolation; `at` is the path prefix for issues.
    fn validate(&self, at: &str, issues: &mut Vec<ConfigIssue>) {
        if let Some(model) = &self.model
            && model.trim().is_empty()
        {
            issues.push(ConfigIssue::new(
                format!("{at}.model"),
                "`model` must not be empty",
            ));
        }
        let dims_path = format!("{at}.dimensions");
        if let Some(dims) = self.dimensions
            && !(MIN_DIMENSIONS..=MAX_DIMENSIONS).contains(&dims)
        {
            issues.push(ConfigIssue::new(
                dims_path.clone(),
                format!(
                    "`dimensions` must be between {MIN_DIMENSIONS} and {MAX_DIMENSIONS} (pgvector halfvec HNSW limit)"
                ),
            ));
            return;
        }
        match (self.preset, self.dimensions) {
            (None, Some(_)) => issues.push(ConfigIssue::new(
                dims_path,
                "`dimensions` requires `preset` (use `preset = \"custom\"` for an arbitrary size)",
            )),
            (Some(EmbeddingPreset::Custom), None) => issues.push(ConfigIssue::new(
                dims_path,
                "preset `custom` requires `dimensions`",
            )),
            (Some(preset), Some(dims)) => {
                if let Some(expected) = preset.dimensions()
                    && expected != dims
                {
                    issues.push(ConfigIssue::new(
                        dims_path,
                        format!(
                            "preset `{}` is {expected} dimensions but `dimensions` is {dims}; remove `dimensions` or use `preset = \"custom\"`",
                            preset.as_str()
                        ),
                    ));
                }
            }
            _ => {}
        }
    }
}

fn validate_excludes(at: &str, patterns: &[String], issues: &mut Vec<ConfigIssue>) {
    for (i, pattern) in patterns.iter().enumerate() {
        if pattern.trim().is_empty() {
            issues.push(ConfigIssue::new(
                format!("{at}[{i}]"),
                "exclude pattern must not be empty",
            ));
        } else if pattern.contains('\0') {
            issues.push(ConfigIssue::new(
                format!("{at}[{i}]"),
                "exclude pattern must not contain NUL",
            ));
        }
    }
}

/// Removes `.` and resolves `..` lexically, without touching the file system.
pub(crate) fn normalize_lexical(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => match out.components().next_back() {
                Some(Component::Normal(_)) => {
                    out.pop();
                }
                Some(Component::RootDir | Component::Prefix(_)) => {}
                _ => out.push(".."),
            },
            other => out.push(other.as_os_str()),
        }
    }
    out
}

/// Issue text for two projects covering the same place; shared by the
/// relative check here and the absolute check in resolution so duplicates
/// collapse into one report.
pub(crate) fn duplicate_location_issue(j: usize, first: usize, first_name: &Name) -> ConfigIssue {
    ConfigIssue::new(
        format!("project[{j}].path"),
        format!("same path and root as project[{first}] (`{first_name}`)"),
    )
}

impl WorkspaceConfig {
    /// Checks every rule that needs no file-system context and returns all
    /// violations (empty when valid). Missing track targets are reported by
    /// [`resolve`](Self::resolve), which sees workspace and project together.
    pub fn validate(&self) -> Vec<ConfigIssue> {
        let mut issues = Vec::new();
        if self.version != SUPPORTED_VERSION {
            issues.push(ConfigIssue::new(
                "version",
                format!(
                    "unsupported version {}; this build understands version {SUPPORTED_VERSION}",
                    self.version
                ),
            ));
        }
        validate_excludes("workspace.exclude", &self.workspace.exclude, &mut issues);
        if let Some(e) = &self.workspace.embedding {
            e.validate("workspace.embedding", &mut issues);
        }

        let mut names: HashMap<&Name, usize> = HashMap::new();
        let mut places: HashMap<(PathBuf, Option<&str>), usize> = HashMap::new();
        for (i, p) in self.project.iter().enumerate() {
            let at = format!("project[{i}]");
            if let Some(first) = names.get(&p.name) {
                issues.push(ConfigIssue::new(
                    format!("{at}.name"),
                    format!(
                        "duplicate project name `{}` (also project[{first}])",
                        p.name
                    ),
                ));
            } else {
                names.insert(&p.name, i);
            }
            if p.path.as_os_str().is_empty() {
                issues.push(ConfigIssue::new(
                    format!("{at}.path"),
                    "`path` must not be empty",
                ));
            } else {
                let key = (
                    normalize_lexical(&p.path),
                    p.root.as_ref().map(RepoPath::as_str),
                );
                match places.get(&key) {
                    Some(&first) => {
                        if let Some(first_project) = self.project.get(first) {
                            issues.push(duplicate_location_issue(i, first, &first_project.name));
                        }
                    }
                    None => {
                        places.insert(key, i);
                    }
                }
            }
            if let Some(remote) = &p.remote
                && remote.trim().is_empty()
            {
                issues.push(ConfigIssue::new(
                    format!("{at}.remote"),
                    "`remote` must not be empty",
                ));
            }
            validate_excludes(&format!("{at}.exclude"), &p.exclude, &mut issues);
            if let Some(e) = &p.embedding {
                e.validate(&format!("{at}.embedding"), &mut issues);
            }
        }
        issues
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{ConfigError, parse_workspace};

    fn issues_of(text: &str) -> Vec<ConfigIssue> {
        match parse_workspace(text) {
            Ok(_) => Vec::new(),
            Err(ConfigError::Invalid { issues, .. }) => issues.0,
            Err(other) => panic!("unexpected error: {other}"),
        }
    }

    fn has(issues: &[ConfigIssue], path: &str) -> bool {
        issues.iter().any(|i| i.path == path)
    }

    const HEAD: &str = "version = 1\n[workspace]\nname = \"shop\"\n";

    #[test]
    fn minimal_workspace_parses() {
        let cfg = parse_workspace(HEAD).unwrap();
        assert_eq!(cfg.workspace.name.as_str(), "shop");
        assert!(cfg.project.is_empty());
    }

    #[test]
    fn version_must_be_one() {
        assert!(has(
            &issues_of("version = 2\n[workspace]\nname = \"a\""),
            "version"
        ));
        assert!(matches!(
            parse_workspace("[workspace]\nname = \"a\""),
            Err(ConfigError::Parse { .. })
        ));
    }

    #[test]
    fn unknown_fields_are_rejected_everywhere() {
        let cases = [
            "version = 1\nextra = 1\n[workspace]\nname = \"a\"",
            "version = 1\n[workspace]\nname = \"a\"\ntrak = \"worktree\"",
            "version = 1\n[workspace]\nname = \"a\"\n[workspace.embedding]\nprovder = \"x\"",
            "version = 1\n[workspace]\nname = \"a\"\n[[project]]\nname = \"p\"\npath = \"p\"\nbranch = \"x\"",
            "version = 1\n[workspace]\nname = \"a\"\n[[project]]\nname = \"p\"\npath = \"p\"\nembedding = { presset = \"compact\" }",
        ];
        for text in cases {
            let err = parse_workspace(text).unwrap_err();
            assert!(err.to_string().contains("unknown field"), "{text}: {err}");
        }
    }

    #[test]
    fn invalid_values_are_parse_errors_with_position() {
        let text = format!("{HEAD}track = \"development\"\n");
        match parse_workspace(&text).unwrap_err() {
            ConfigError::Parse { line, column, .. } => {
                assert_eq!(line, Some(4));
                assert!(column.is_some());
            }
            other => panic!("{other}"),
        }
        assert!(parse_workspace(&format!("{HEAD}data_policy = \"public\"")).is_err());
        assert!(parse_workspace("version = 1\n[workspace]\nname = \"Bad Name\"").is_err());
    }

    #[test]
    fn duplicate_names_and_locations() {
        let text = format!(
            "{HEAD}[[project]]\nname = \"a\"\npath = \"x\"\n[[project]]\nname = \"a\"\npath = \"y\"\n[[project]]\nname = \"b\"\npath = \"./x\"\n[[project]]\nname = \"c\"\npath = \"x\"\nroot = \"sub\"\n"
        );
        let issues = issues_of(&text);
        assert!(has(&issues, "project[1].name"));
        assert!(has(&issues, "project[2].path"));
        assert!(!has(&issues, "project[3].path"), "different root is fine");
        assert_eq!(issues.len(), 2, "{issues:?}");
    }

    #[test]
    fn same_path_with_different_roots_is_allowed() {
        let text = format!(
            "{HEAD}[[project]]\nname = \"a\"\npath = \"mono\"\nroot = \"svc/a\"\n[[project]]\nname = \"b\"\npath = \"mono\"\nroot = \"svc/b\"\n"
        );
        assert!(issues_of(&text).is_empty());
    }

    #[test]
    fn empty_values() {
        let text = format!(
            "{HEAD}exclude = [\"\", \"a\\u0000b\"]\n[[project]]\nname = \"a\"\npath = \"\"\nremote = \" \"\nexclude = [\" \"]\n"
        );
        let issues = issues_of(&text);
        for path in [
            "workspace.exclude[0]",
            "workspace.exclude[1]",
            "project[0].path",
            "project[0].remote",
            "project[0].exclude[0]",
        ] {
            assert!(has(&issues, path), "missing {path}: {issues:?}");
        }
    }

    fn embedding_issues(block: &str) -> Vec<ConfigIssue> {
        issues_of(&format!("{HEAD}[workspace.embedding]\n{block}\n"))
    }

    #[test]
    fn preset_and_dimension_combinations() {
        assert!(embedding_issues("preset = \"compact\"").is_empty());
        assert!(embedding_issues("preset = \"compact\"\ndimensions = 768").is_empty());
        assert!(embedding_issues("preset = \"extended\"\ndimensions = 3072").is_empty());
        assert!(embedding_issues("preset = \"custom\"\ndimensions = 1024").is_empty());
        assert!(embedding_issues("preset = \"custom\"\ndimensions = 32").is_empty());
        assert!(embedding_issues("preset = \"custom\"\ndimensions = 4000").is_empty());

        let path = "workspace.embedding.dimensions";
        for block in [
            "preset = \"custom\"",
            "preset = \"compact\"\ndimensions = 1536",
            "dimensions = 768",
            "preset = \"custom\"\ndimensions = 31",
            "preset = \"custom\"\ndimensions = 4001",
            "preset = \"custom\"\ndimensions = 0",
        ] {
            let issues = embedding_issues(block);
            assert!(has(&issues, path), "{block}: {issues:?}");
            assert_eq!(issues.len(), 1, "{block}: {issues:?}");
        }
        assert!(
            parse_workspace(&format!("{HEAD}[workspace.embedding]\npreset = \"huge\"")).is_err()
        );
    }

    #[test]
    fn project_embedding_is_validated_with_its_index() {
        let text = format!(
            "{HEAD}[[project]]\nname = \"a\"\npath = \"a\"\n[[project]]\nname = \"b\"\npath = \"b\"\nembedding = {{ preset = \"custom\", model = \"\" }}\n"
        );
        let issues = issues_of(&text);
        assert!(has(&issues, "project[1].embedding.dimensions"));
        assert!(has(&issues, "project[1].embedding.model"));
    }

    #[test]
    fn lexical_normalisation() {
        assert_eq!(normalize_lexical(Path::new("./a/../b")), PathBuf::from("b"));
        assert_eq!(normalize_lexical(Path::new("../a")), PathBuf::from("../a"));
        assert_eq!(normalize_lexical(Path::new("a/b/../..")), PathBuf::new());
    }
}
