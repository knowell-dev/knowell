//! Resolution of workspace → project inheritance, with provenance.
//!
//! Every inheritable setting of a resolved project records where its value
//! came from, so the panel can show "development (from workspace)".

use std::collections::{HashMap, HashSet};
use std::fmt;
use std::path::{Path, PathBuf};

use knowell_core::{Name, RepoPath, TrackTarget};
use serde::Serialize;

use crate::engine::EngineConfig;
use crate::error::{ConfigIssue, ConfigIssues};
use crate::workspace::{
    DataPolicy, EmbeddingConfig, EmbeddingPreset, WorkspaceConfig, duplicate_location_issue,
    normalize_lexical,
};

/// Where a resolved setting came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Origin {
    /// Knowell's built-in default (nothing was configured).
    Builtin,
    /// The `[workspace]` table.
    Workspace,
    /// The project's own table.
    Project,
}

impl fmt::Display for Origin {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Origin::Builtin => "built-in default",
            Origin::Workspace => "workspace",
            Origin::Project => "project",
        })
    }
}

/// A setting together with the place it was defined.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Sourced<T> {
    /// The effective value.
    pub value: T,
    /// Where the value came from.
    pub origin: Origin,
}

impl<T> Sourced<T> {
    fn new(value: T, origin: Origin) -> Self {
        Self { value, origin }
    }
}

/// Effective embedding settings of one project.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ResolvedEmbedding {
    /// Provider name (key in the engine config's `providers`). `None` when
    /// no layer names one: the project then has lexical and graph search only.
    pub provider: Option<Sourced<Name>>,
    /// Model named by the workspace or project; `None` means "use the
    /// provider's default model".
    pub model: Option<Sourced<String>>,
    /// Vector size preset (default `balanced`).
    pub preset: Sourced<EmbeddingPreset>,
    /// Vector size in dimensions, 32..=4000. Shares its origin with `preset`.
    pub dimensions: Sourced<u32>,
}

/// A project with every inherited setting filled in.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ResolvedProject {
    /// Project name.
    pub name: Name,
    /// Absolute, lexically normalised directory of the project (symlinks are
    /// not resolved and the directory is not required to exist).
    pub path: PathBuf,
    /// Optional sub-root inside `path` for monorepo projects.
    pub root: Option<RepoPath>,
    /// Informational clone URL.
    pub remote: Option<String>,
    /// Ref the project follows. Never defaulted: always `Project` or `Workspace` origin.
    pub track: Sourced<TrackTarget>,
    /// Data policy (built-in default `local-only`).
    pub data_policy: Sourced<DataPolicy>,
    /// Embedding settings.
    pub embedding: ResolvedEmbedding,
    /// Effective exclude globs: workspace patterns first, then the project's,
    /// each with its own origin. Repeated patterns keep their first origin.
    pub exclude: Vec<Sourced<String>>,
}

/// A workspace after inheritance, ready to be checked against an engine config.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ResolvedWorkspace {
    /// Workspace name.
    pub name: Name,
    /// Workspace description.
    pub description: Option<String>,
    /// Projects in file order.
    pub projects: Vec<ResolvedProject>,
}

impl WorkspaceConfig {
    /// Validates and resolves inheritance. Relative project paths are joined
    /// onto `base_dir` (the directory holding `knowell.toml`).
    ///
    /// Returns every problem at once, including projects that end up without
    /// a track target: Knowell never assumes a default branch.
    pub fn resolve(&self, base_dir: &Path) -> Result<ResolvedWorkspace, ConfigIssues> {
        let mut issues = self.validate();
        let mut projects = Vec::with_capacity(self.project.len());
        let mut places: HashMap<(PathBuf, Option<&str>), usize> = HashMap::new();
        let ws = &self.workspace;

        for (i, p) in self.project.iter().enumerate() {
            let track = match (&p.track, &ws.track) {
                (Some(t), _) => Some(Sourced::new(t.clone(), Origin::Project)),
                (None, Some(t)) => Some(Sourced::new(t.clone(), Origin::Workspace)),
                (None, None) => {
                    issues.push(ConfigIssue::new(
                        format!("project[{i}].track"),
                        format!(
                            "project `{}` has no track target; set `track` on the project or `workspace.track` (Knowell never assumes a default branch)",
                            p.name
                        ),
                    ));
                    None
                }
            };

            let data_policy = match (p.data_policy, ws.data_policy) {
                (Some(v), _) => Sourced::new(v, Origin::Project),
                (None, Some(v)) => Sourced::new(v, Origin::Workspace),
                (None, None) => Sourced::new(DataPolicy::default(), Origin::Builtin),
            };

            let embedding = resolve_embedding(ws.embedding.as_ref(), p.embedding.as_ref());

            let mut exclude: Vec<Sourced<String>> = Vec::new();
            let mut seen: HashSet<&str> = HashSet::new();
            let layers = [
                (&ws.exclude, Origin::Workspace),
                (&p.exclude, Origin::Project),
            ];
            for (patterns, origin) in layers {
                for pattern in patterns {
                    if seen.insert(pattern.as_str()) {
                        exclude.push(Sourced::new(pattern.clone(), origin));
                    }
                }
            }

            let joined = if p.path.is_absolute() {
                p.path.clone()
            } else {
                base_dir.join(&p.path)
            };
            let path = match std::path::absolute(&joined) {
                Ok(abs) => normalize_lexical(&abs),
                Err(_) => {
                    issues.push(ConfigIssue::new(
                        format!("project[{i}].path"),
                        "cannot make the path absolute; pass an absolute base directory",
                    ));
                    continue;
                }
            };

            let key = (path.clone(), p.root.as_ref().map(RepoPath::as_str));
            if let Some(&first) = places.get(&key) {
                if let Some(first_project) = self.project.get(first) {
                    issues.push(duplicate_location_issue(i, first, &first_project.name));
                }
            } else {
                places.insert(key, i);
            }

            if let Some(track) = track {
                projects.push(ResolvedProject {
                    name: p.name.clone(),
                    path,
                    root: p.root.clone(),
                    remote: p.remote.clone(),
                    track,
                    data_policy,
                    embedding,
                    exclude,
                });
            }
        }

        // The relative and absolute duplicate checks can report the same pair.
        let mut reported = HashSet::new();
        issues.retain(|issue| reported.insert((issue.path.clone(), issue.message.clone())));

        if issues.is_empty() {
            Ok(ResolvedWorkspace {
                name: ws.name.clone(),
                description: ws.description.clone(),
                projects,
            })
        } else {
            Err(ConfigIssues(issues))
        }
    }
}

/// Merges embedding layers field by field; `preset` and `dimensions` move as
/// a unit from the highest layer that sets either. Layer problems (such as
/// `dimensions` without `preset`) were already reported by validation; this
/// still returns a well-formed value in that case.
fn resolve_embedding(
    ws: Option<&EmbeddingConfig>,
    project: Option<&EmbeddingConfig>,
) -> ResolvedEmbedding {
    let layers = [(project, Origin::Project), (ws, Origin::Workspace)];

    let pick = |get: fn(&EmbeddingConfig) -> Option<&Name>| {
        layers.iter().find_map(|(layer, origin)| {
            layer
                .and_then(get)
                .map(|v| Sourced::new(v.clone(), *origin))
        })
    };
    let provider = pick(|e| e.provider.as_ref());
    let model = layers.iter().find_map(|(layer, origin)| {
        layer
            .and_then(|e| e.model.as_ref())
            .map(|v| Sourced::new(v.clone(), *origin))
    });

    let (preset, dimensions) = layers
        .iter()
        .find_map(|(layer, origin)| {
            let layer = (*layer)?;
            if layer.preset.is_none() && layer.dimensions.is_none() {
                return None;
            }
            let preset = layer.preset.unwrap_or_default();
            let dims = layer
                .dimensions
                .or_else(|| preset.dimensions())
                .unwrap_or(EmbeddingPreset::Balanced.dimensions().unwrap_or(1536));
            Some((Sourced::new(preset, *origin), Sourced::new(dims, *origin)))
        })
        .unwrap_or_else(|| {
            let preset = EmbeddingPreset::default();
            (
                Sourced::new(preset, Origin::Builtin),
                Sourced::new(preset.dimensions().unwrap_or(1536), Origin::Builtin),
            )
        });

    ResolvedEmbedding {
        provider,
        model,
        preset,
        dimensions,
    }
}

impl ResolvedWorkspace {
    /// Cross-checks the resolved workspace against the engine config and
    /// returns every problem found (empty when consistent).
    ///
    /// Reported: embedding providers that the engine config does not define,
    /// `local-only` projects that name a cloud provider (such a project could
    /// never embed anything), and embeddings without any model.
    pub fn check_against(&self, engine: &EngineConfig) -> Vec<ConfigIssue> {
        let mut issues = Vec::new();
        for (i, project) in self.projects.iter().enumerate() {
            let Some(provider_ref) = &project.embedding.provider else {
                continue;
            };
            let at = format!("project[{i}].embedding.provider");
            let Some(provider) = engine.providers.get(&provider_ref.value) else {
                issues.push(ConfigIssue::new(
                    at,
                    format!(
                        "project `{}` uses embedding provider `{}` (from {}), which is not defined under `[providers]` in the engine config",
                        project.name, provider_ref.value, provider_ref.origin
                    ),
                ));
                continue;
            };
            if project.data_policy.value == DataPolicy::LocalOnly && provider.kind.is_cloud() {
                issues.push(ConfigIssue::new(
                    at.clone(),
                    format!(
                        "project `{}` has data_policy `local-only` (from {}) but embedding provider `{}` is a cloud service (kind `{}`); nothing could ever be embedded. Set data_policy = \"cloud\" to allow it, or use a local provider such as ollama",
                        project.name,
                        project.data_policy.origin,
                        provider_ref.value,
                        provider.kind.as_str()
                    ),
                ));
            }
            if project.embedding.model.is_none() && provider.model.is_none() {
                issues.push(ConfigIssue::new(
                    format!("project[{i}].embedding.model"),
                    format!(
                        "project `{}` has no embedding model: set `model` in the workspace or project embedding, or on provider `{}`",
                        project.name, provider_ref.value
                    ),
                ));
            }
        }
        issues
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parse_engine;

    #[cfg(windows)]
    const BASE: &str = "C:\\work\\shop";
    #[cfg(not(windows))]
    const BASE: &str = "/work/shop";

    fn resolve(text: &str) -> Result<ResolvedWorkspace, ConfigIssues> {
        // Deserialise without `parse_workspace`: resolve() validates on its own,
        // and tests want to see its combined report.
        let cfg: WorkspaceConfig = toml::from_str(text).unwrap();
        cfg.resolve(Path::new(BASE))
    }

    fn ws(extra_ws: &str, projects: &str) -> String {
        format!("version = 1\n[workspace]\nname = \"shop\"\n{extra_ws}\n{projects}")
    }

    #[test]
    fn track_inheritance_and_override() {
        let r = resolve(&ws(
            "track = \"branch:development\"",
            "[[project]]\nname = \"a\"\npath = \"a\"\n[[project]]\nname = \"b\"\npath = \"b\"\ntrack = \"remote:origin/release/2.x\"\n",
        ))
        .unwrap();
        let a = r.projects.first().unwrap();
        let b = r.projects.get(1).unwrap();
        assert_eq!(a.track.value.to_string(), "branch:development");
        assert_eq!(a.track.origin, Origin::Workspace);
        assert_eq!(b.track.value.to_string(), "remote:origin/release/2.x");
        assert_eq!(b.track.origin, Origin::Project);
    }

    #[test]
    fn missing_track_is_an_error_naming_the_project() {
        let err = resolve(&ws(
            "",
            "[[project]]\nname = \"a\"\npath = \"a\"\ntrack = \"worktree\"\n[[project]]\nname = \"lonely\"\npath = \"l\"\n",
        ))
        .unwrap_err();
        assert_eq!(err.len(), 1);
        let issue = err.iter().next().unwrap();
        assert_eq!(issue.path, "project[1].track");
        assert!(issue.message.contains("`lonely`"));
        assert!(issue.message.contains("default branch"));
    }

    #[test]
    fn resolve_reports_validation_and_track_issues_together() {
        let err = resolve(&ws(
            "",
            "[[project]]\nname = \"a\"\npath = \"a\"\n[[project]]\nname = \"a\"\npath = \"b\"\n",
        ))
        .unwrap_err();
        assert!(err.iter().any(|i| i.path == "project[1].name"));
        assert!(err.iter().any(|i| i.path == "project[0].track"));
        assert!(err.iter().any(|i| i.path == "project[1].track"));
    }

    #[test]
    fn data_policy_defaults_to_local_only_with_provenance() {
        let r = resolve(&ws(
            "track = \"worktree\"",
            "[[project]]\nname = \"a\"\npath = \"a\"\n",
        ))
        .unwrap();
        let p = r.projects.first().unwrap();
        assert_eq!(p.data_policy.value, DataPolicy::LocalOnly);
        assert_eq!(p.data_policy.origin, Origin::Builtin);

        let r = resolve(&ws(
            "track = \"worktree\"\ndata_policy = \"cloud\"",
            "[[project]]\nname = \"a\"\npath = \"a\"\n[[project]]\nname = \"b\"\npath = \"b\"\ndata_policy = \"local-only\"\n",
        ))
        .unwrap();
        let a = r.projects.first().unwrap();
        let b = r.projects.get(1).unwrap();
        assert_eq!(
            (a.data_policy.value, a.data_policy.origin),
            (DataPolicy::Cloud, Origin::Workspace)
        );
        assert_eq!(
            (b.data_policy.value, b.data_policy.origin),
            (DataPolicy::LocalOnly, Origin::Project)
        );
    }

    #[test]
    fn excludes_are_appended_with_per_pattern_origin() {
        let r = resolve(&ws(
            "track = \"worktree\"\nexclude = [\"**/generated/**\", \"dist/**\"]",
            "[[project]]\nname = \"a\"\npath = \"a\"\nexclude = [\"fixtures/**\", \"dist/**\"]\n",
        ))
        .unwrap();
        let ex: Vec<_> = r
            .projects
            .first()
            .unwrap()
            .exclude
            .iter()
            .map(|s| (s.value.as_str(), s.origin))
            .collect();
        assert_eq!(
            ex,
            [
                ("**/generated/**", Origin::Workspace),
                ("dist/**", Origin::Workspace),
                ("fixtures/**", Origin::Project),
            ]
        );
    }

    #[test]
    fn embedding_defaults_to_balanced_builtin_without_provider() {
        let r = resolve(&ws(
            "track = \"worktree\"",
            "[[project]]\nname = \"a\"\npath = \"a\"\n",
        ))
        .unwrap();
        let e = &r.projects.first().unwrap().embedding;
        assert!(e.provider.is_none());
        assert!(e.model.is_none());
        assert_eq!(
            e.preset,
            Sourced::new(EmbeddingPreset::Balanced, Origin::Builtin)
        );
        assert_eq!(e.dimensions, Sourced::new(1536, Origin::Builtin));
    }

    #[test]
    fn embedding_merges_field_by_field() {
        let r = resolve(&ws(
            "track = \"worktree\"\n[workspace.embedding]\nprovider = \"gemini\"\nmodel = \"m1\"\npreset = \"extended\"",
            "[[project]]\nname = \"a\"\npath = \"a\"\n[[project]]\nname = \"b\"\npath = \"b\"\nembedding = { preset = \"compact\", model = \"m2\" }\n",
        ))
        .unwrap();
        let a = &r.projects.first().unwrap().embedding;
        assert_eq!(a.provider.as_ref().unwrap().origin, Origin::Workspace);
        assert_eq!(a.model.as_ref().unwrap().value, "m1");
        assert_eq!(
            a.preset,
            Sourced::new(EmbeddingPreset::Extended, Origin::Workspace)
        );
        assert_eq!(a.dimensions, Sourced::new(3072, Origin::Workspace));

        let b = &r.projects.get(1).unwrap().embedding;
        assert_eq!(b.provider.as_ref().unwrap().value.as_str(), "gemini");
        assert_eq!(b.provider.as_ref().unwrap().origin, Origin::Workspace);
        assert_eq!(
            b.model,
            Some(Sourced::new("m2".to_owned(), Origin::Project))
        );
        assert_eq!(
            b.preset,
            Sourced::new(EmbeddingPreset::Compact, Origin::Project)
        );
        assert_eq!(b.dimensions, Sourced::new(768, Origin::Project));
    }

    #[test]
    fn project_preset_replaces_workspace_custom_dimensions() {
        let r = resolve(&ws(
            "track = \"worktree\"\n[workspace.embedding]\npreset = \"custom\"\ndimensions = 2000",
            "[[project]]\nname = \"a\"\npath = \"a\"\nembedding = { preset = \"compact\" }\n[[project]]\nname = \"b\"\npath = \"b\"\n",
        ))
        .unwrap();
        let a = &r.projects.first().unwrap().embedding;
        assert_eq!(a.dimensions, Sourced::new(768, Origin::Project));
        let b = &r.projects.get(1).unwrap().embedding;
        assert_eq!(
            b.preset,
            Sourced::new(EmbeddingPreset::Custom, Origin::Workspace)
        );
        assert_eq!(b.dimensions, Sourced::new(2000, Origin::Workspace));
    }

    #[test]
    fn paths_become_absolute_and_normalised() {
        let r = resolve(&ws(
            "track = \"worktree\"",
            "[[project]]\nname = \"a\"\npath = \"./svc/../a\"\nroot = \"packages/x\"\nremote = \"https://example.com/a.git\"\n",
        ))
        .unwrap();
        let p = r.projects.first().unwrap();
        assert!(p.path.is_absolute());
        assert_eq!(p.path, normalize_lexical(&Path::new(BASE).join("a")));
        assert_eq!(p.root.as_ref().unwrap().as_str(), "packages/x");
        assert_eq!(p.remote.as_deref(), Some("https://example.com/a.git"));
    }

    #[test]
    fn absolute_and_relative_spelling_of_one_directory_collide() {
        let abs = Path::new(BASE).join("a");
        let abs = abs.to_string_lossy().replace('\\', "/");
        let text = ws(
            "track = \"worktree\"",
            &format!(
                "[[project]]\nname = \"a\"\npath = \"a\"\n[[project]]\nname = \"b\"\npath = \"{abs}\"\n"
            ),
        );
        let err = resolve(&text).unwrap_err();
        assert_eq!(err.len(), 1, "{err}");
        assert_eq!(err.iter().next().unwrap().path, "project[1].path");
    }

    fn engine(text: &str) -> EngineConfig {
        parse_engine(text).unwrap()
    }

    const ENGINE: &str = "version = 1\n[providers.gemini]\nkind = \"gemini\"\napi_key = \"env:GEMINI_API_KEY\"\n[providers.local]\nkind = \"ollama\"\n";

    #[test]
    fn check_against_reports_unknown_provider() {
        let r = resolve(&ws(
            "track = \"worktree\"\n[workspace.embedding]\nprovider = \"missing\"\nmodel = \"m\"",
            "[[project]]\nname = \"a\"\npath = \"a\"\n",
        ))
        .unwrap();
        let issues = r.check_against(&engine(ENGINE));
        assert_eq!(issues.len(), 1);
        assert_eq!(
            issues.first().unwrap().path,
            "project[0].embedding.provider"
        );
        assert!(issues.first().unwrap().message.contains("`missing`"));
    }

    #[test]
    fn check_against_reports_local_only_with_cloud_provider() {
        let r = resolve(&ws(
            "track = \"worktree\"\n[workspace.embedding]\nprovider = \"gemini\"\nmodel = \"m\"",
            "[[project]]\nname = \"a\"\npath = \"a\"\n[[project]]\nname = \"b\"\npath = \"b\"\ndata_policy = \"cloud\"\n",
        ))
        .unwrap();
        let issues = r.check_against(&engine(ENGINE));
        assert_eq!(issues.len(), 1, "{issues:?}");
        let msg = &issues.first().unwrap().message;
        assert!(msg.contains("local-only") && msg.contains("built-in default"));
        assert!(msg.contains("`a`"));
    }

    #[test]
    fn check_against_accepts_local_provider_and_cloud_with_opt_in() {
        let r = resolve(&ws(
            "track = \"worktree\"\ndata_policy = \"cloud\"",
            "[[project]]\nname = \"a\"\npath = \"a\"\nembedding = { provider = \"gemini\", model = \"m\" }\n[[project]]\nname = \"b\"\npath = \"b\"\ndata_policy = \"local-only\"\nembedding = { provider = \"local\", model = \"nomic\" }\n",
        ))
        .unwrap();
        assert!(r.check_against(&engine(ENGINE)).is_empty());
    }

    #[test]
    fn check_against_reports_missing_model() {
        let r = resolve(&ws(
            "track = \"worktree\"\n[workspace.embedding]\nprovider = \"local\"",
            "[[project]]\nname = \"a\"\npath = \"a\"\n",
        ))
        .unwrap();
        let issues = r.check_against(&engine(ENGINE));
        assert_eq!(issues.len(), 1);
        assert_eq!(issues.first().unwrap().path, "project[0].embedding.model");
        let with_default = "version = 1\n[providers.local]\nkind = \"ollama\"\nmodel = \"nomic\"\n";
        assert!(r.check_against(&engine(with_default)).is_empty());
    }

    #[test]
    fn projects_without_provider_need_no_engine_support() {
        let r = resolve(&ws(
            "track = \"worktree\"",
            "[[project]]\nname = \"a\"\npath = \"a\"\n",
        ))
        .unwrap();
        assert!(r.check_against(&engine("version = 1")).is_empty());
    }
}
