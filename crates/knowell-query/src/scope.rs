use std::collections::{BTreeMap, BTreeSet};

use knowell_core::{Name, RepoPath};
use serde::{Deserialize, Serialize};

use crate::{CommitId, Language, Layer, PathGlob, QueryError, ViewId};

/// One view pinned at query start: which view, which index generation, and
/// (for git sources) which commit that generation was built from.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PinnedView {
    /// The view.
    pub view: ViewId,
    /// Active index generation of the view when the query started. Evidence
    /// from any other generation is dropped.
    pub generation: u64,
    /// Commit the generation was built from; `None` for non-git sources.
    pub commit: Option<CommitId>,
}

/// The user's personal worktree layer over a project's base view.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct OverlayPin {
    /// The overlay view and its generation.
    pub pin: PinnedView,
    /// Paths changed or deleted in the worktree relative to the base view.
    /// Base-view evidence for these paths is stale for this user and is
    /// dropped even when the overlay has no candidate for the path.
    #[serde(default)]
    pub shadowed_paths: BTreeSet<RepoPath>,
}

/// What analysis a project's pinned generation actually has, so the engine
/// can say what it could not have found.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProjectCoverage {
    /// Languages with indexed text in the pinned generation.
    #[serde(default)]
    pub languages: BTreeSet<Language>,
    /// Languages with reference resolution (SCIP or a language tool); callers,
    /// callees and impact for other languages rest on syntax or heuristics.
    #[serde(default)]
    pub reference_resolution: BTreeSet<Language>,
}

/// The pinned state of one project.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProjectPin {
    /// The tracked view.
    pub base: PinnedView,
    /// The user's worktree layer, if one is active.
    #[serde(default)]
    pub overlay: Option<OverlayPin>,
    /// Analysis coverage of the pinned generation.
    #[serde(default)]
    pub coverage: ProjectCoverage,
}

impl ProjectPin {
    /// The pinned view with id `view`, and which layer it is.
    pub fn view(&self, view: &ViewId) -> Option<(&PinnedView, Layer)> {
        if self.base.view == *view {
            return Some((&self.base, Layer::Base));
        }
        self.overlay
            .as_ref()
            .filter(|overlay| overlay.pin.view == *view)
            .map(|overlay| (&overlay.pin, Layer::Overlay))
    }
}

/// Every project's view generation and commit, pinned when the query starts,
/// so that a commit landing mid-search cannot make a result half old and half
/// new. Evidence that does not belong to a pinned view is dropped and counted.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ViewManifest {
    /// Workspace the manifest was pinned for.
    pub workspace: Name,
    /// Pinned projects.
    #[serde(default)]
    pub projects: BTreeMap<Name, ProjectPin>,
    /// Projects that belong to the workspace but have no active index yet.
    #[serde(default)]
    pub not_indexed: BTreeSet<Name>,
}

impl ViewManifest {
    /// An empty manifest for `workspace`.
    pub fn new(workspace: Name) -> Self {
        Self {
            workspace,
            projects: BTreeMap::new(),
            not_indexed: BTreeSet::new(),
        }
    }

    /// Checks internal consistency: an overlay must be a different view than
    /// its base, and a project cannot be both pinned and unindexed.
    pub fn validate(&self) -> Result<(), QueryError> {
        for (project, pin) in &self.projects {
            if let Some(overlay) = &pin.overlay
                && overlay.pin.view == pin.base.view
            {
                return Err(QueryError::InvalidManifest {
                    project: project.clone(),
                    reason: "overlay view must differ from the base view",
                });
            }
            if self.not_indexed.contains(project) {
                return Err(QueryError::InvalidManifest {
                    project: project.clone(),
                    reason: "project is both pinned and listed as not indexed",
                });
            }
        }
        Ok(())
    }
}

/// Include / exclude path patterns. A path is in scope when it matches at
/// least one include pattern (or there are none) and no exclude pattern.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PathFilter {
    /// Patterns a path must match (any of); empty means every path.
    #[serde(default)]
    pub include: Vec<PathGlob>,
    /// Patterns a path must not match.
    #[serde(default)]
    pub exclude: Vec<PathGlob>,
}

impl PathFilter {
    /// Whether `path` passes the filter.
    pub fn admits(&self, path: &RepoPath) -> bool {
        let included = self.include.is_empty() || self.include.iter().any(|g| g.matches(path));
        included && !self.exclude.iter().any(|g| g.matches(path))
    }

    /// Whether the filter restricts anything.
    pub fn is_unrestricted(&self) -> bool {
        self.include.is_empty() && self.exclude.is_empty()
    }
}

/// The authorised, user-narrowed space one query searches.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct QueryScope {
    /// Workspace searched.
    pub workspace: Name,
    /// Projects to search; `None` searches every pinned project.
    #[serde(default)]
    pub projects: Option<BTreeSet<Name>>,
    /// Languages to search; `None` searches every language. Evidence without a
    /// known language is dropped when this is set.
    #[serde(default)]
    pub languages: Option<BTreeSet<Language>>,
    /// Path patterns.
    #[serde(default)]
    pub paths: PathFilter,
    /// Business domain; selects domain-specific glossary entries and is passed
    /// to sources that can filter by it.
    #[serde(default)]
    pub domain: Option<Name>,
    /// Views pinned at query start.
    pub manifest: ViewManifest,
}

/// Why a piece of evidence was not admitted.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Rejection {
    UnpinnedProject,
    ViewMismatch,
    ProjectFilter,
    LanguageFilter,
    PathFilter,
}

impl QueryScope {
    /// A scope covering every pinned project of `manifest`'s workspace.
    pub fn all(manifest: ViewManifest) -> Self {
        Self {
            workspace: manifest.workspace.clone(),
            projects: None,
            languages: None,
            paths: PathFilter::default(),
            domain: None,
            manifest,
        }
    }

    /// Checks that the scope and its manifest agree.
    pub fn validate(&self) -> Result<(), QueryError> {
        if self.workspace != self.manifest.workspace {
            return Err(QueryError::WorkspaceMismatch {
                scope: self.workspace.clone(),
                manifest: self.manifest.workspace.clone(),
            });
        }
        self.manifest.validate()
    }

    /// Pinned projects selected by the project filter, in name order.
    pub fn searched_projects(&self) -> impl Iterator<Item = (&Name, &ProjectPin)> {
        self.manifest
            .projects
            .iter()
            .filter(|(name, _)| self.project_selected(name))
    }

    fn project_selected(&self, project: &Name) -> bool {
        self.projects
            .as_ref()
            .is_none_or(|selected| selected.contains(project))
    }

    /// The pinned view for `project` / `view`, if any.
    pub fn pinned(&self, project: &Name, view: &ViewId) -> Option<(&PinnedView, Layer)> {
        self.manifest.projects.get(project)?.view(view)
    }

    /// Admission check shared by candidates and graph neighbours. The manifest
    /// is checked first (evidence outside pinned views is never in scope),
    /// then the user's filters.
    pub(crate) fn admit(
        &self,
        project: &Name,
        view: &ViewId,
        generation: u64,
        path: &RepoPath,
        language: Option<&Language>,
    ) -> Result<Layer, Rejection> {
        let Some(pin) = self.manifest.projects.get(project) else {
            return Err(Rejection::UnpinnedProject);
        };
        let Some((pinned, layer)) = pin.view(view) else {
            return Err(Rejection::ViewMismatch);
        };
        if pinned.generation != generation {
            return Err(Rejection::ViewMismatch);
        }
        if !self.project_selected(project) {
            return Err(Rejection::ProjectFilter);
        }
        if let Some(languages) = &self.languages {
            let known = language.is_some_and(|l| languages.contains(l));
            if !known {
                return Err(Rejection::LanguageFilter);
            }
        }
        if !self.paths.admits(path) {
            return Err(Rejection::PathFilter);
        }
        Ok(layer)
    }

    /// Whether base-view evidence for `path` is shadowed by the project's
    /// overlay declaration (changed or deleted in the worktree).
    pub(crate) fn declared_shadowed(&self, project: &Name, path: &RepoPath) -> bool {
        self.manifest
            .projects
            .get(project)
            .and_then(|pin| pin.overlay.as_ref())
            .is_some_and(|overlay| overlay.shadowed_paths.contains(path))
    }
}
