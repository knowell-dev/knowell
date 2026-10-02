//! Grouping worktrees of several repositories into task views.
//!
//! A task usually spans several repositories: one linked worktree per
//! repository, all on the same branch or all below one task folder such as
//! `<root>/.worktree/<slug>/<module>`. [`group_task_views`] joins such
//! worktrees into one [`TaskView`] per task.

use std::collections::BTreeMap;
use std::path::{Component, Path};

use serde::{Deserialize, Serialize};

use super::WorktreeInfo;

/// Error returned when a worktree path pattern is invalid.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum PatternError {
    /// The pattern has no components.
    #[error("worktree pattern must not be empty")]
    Empty,
    /// `{slug}` must appear exactly once.
    #[error("worktree pattern `{0}` must contain `{{slug}}` exactly once as a whole component")]
    Slug(String),
    /// A component is empty, `.`, `..`, or uses an unknown placeholder.
    #[error(
        "worktree pattern `{0}` may only contain names, `*`, `{{slug}}`, `{{module}}` and a leading `{{root}}`"
    )]
    Component(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Segment {
    Literal(String),
    Slug,
    Any,
}

/// Where a task slug sits in a worktree path, for example
/// `{root}/.worktree/{slug}/{module}` or `.worktree/{slug}/*`.
///
/// Components are separated by `/`. `{slug}` (exactly once) captures the
/// slug; `{module}` and `*` match any single component; other components
/// match literally and case-sensitively. A leading `{root}` is optional and
/// means "anything before": the pattern is always matched against the
/// **last** components of the worktree path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PathPattern {
    source: String,
    segments: Vec<Segment>,
}

impl PathPattern {
    /// Parses a pattern.
    ///
    /// # Errors
    /// [`PatternError`] for an empty pattern, a missing or repeated
    /// `{slug}`, or an invalid component.
    pub fn new(pattern: &str) -> Result<Self, PatternError> {
        let mut parts: Vec<&str> = pattern.split('/').collect();
        if parts.first() == Some(&"{root}") {
            parts.remove(0);
        }
        if parts.is_empty() || parts.iter().all(|p| p.is_empty()) {
            return Err(PatternError::Empty);
        }
        let mut segments = Vec::with_capacity(parts.len());
        for part in parts {
            let segment = match part {
                "{slug}" => Segment::Slug,
                "{module}" | "*" => Segment::Any,
                "" | "." | ".." => return Err(PatternError::Component(pattern.to_owned())),
                literal if literal.contains(['{', '}', '*', '\\']) => {
                    return Err(PatternError::Component(pattern.to_owned()));
                }
                literal => Segment::Literal(literal.to_owned()),
            };
            segments.push(segment);
        }
        if segments.iter().filter(|s| **s == Segment::Slug).count() != 1 {
            return Err(PatternError::Slug(pattern.to_owned()));
        }
        Ok(Self {
            source: pattern.to_owned(),
            segments,
        })
    }

    /// The pattern as written.
    pub fn as_str(&self) -> &str {
        &self.source
    }

    /// The slug captured from `path`, if the last components of `path`
    /// match the pattern. Non-UTF-8 components never match.
    pub fn slug_of(&self, path: &Path) -> Option<String> {
        let names: Vec<&str> = path
            .components()
            .filter_map(|c| match c {
                Component::Normal(name) => Some(name.to_str()),
                _ => None,
            })
            .collect::<Option<Vec<_>>>()?;
        let start = names.len().checked_sub(self.segments.len())?;
        let tail = names.get(start..)?;
        let mut slug = None;
        for (segment, name) in self.segments.iter().zip(tail) {
            match segment {
                Segment::Literal(literal) if literal != name => return None,
                Segment::Literal(_) | Segment::Any => {}
                Segment::Slug => slug = Some((*name).to_owned()),
            }
        }
        slug
    }
}

impl Default for PathPattern {
    /// `.worktree/{slug}/*`.
    fn default() -> Self {
        Self {
            source: ".worktree/{slug}/*".to_owned(),
            segments: vec![
                Segment::Literal(".worktree".to_owned()),
                Segment::Slug,
                Segment::Any,
            ],
        }
    }
}

/// How worktrees are joined into task views.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TaskGrouping {
    /// Path layout that names the task; `None` disables path grouping.
    /// Default: `.worktree/{slug}/*`.
    pub path_pattern: Option<PathPattern>,
    /// Join worktrees that have the same branch checked out. Default `true`.
    pub by_branch: bool,
    /// Also consider main worktrees. Default `false`: a main worktree is the
    /// shared checkout, and main worktrees on a common branch such as
    /// `development` are not one task.
    pub include_main: bool,
}

impl Default for TaskGrouping {
    fn default() -> Self {
        Self {
            path_pattern: Some(PathPattern::default()),
            by_branch: true,
            include_main: false,
        }
    }
}

/// One worktree in a task view.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TaskMember {
    /// Caller-chosen repository (project) name.
    pub repo: String,
    /// The worktree.
    pub worktree: WorktreeInfo,
}

/// Worktrees of one or more repositories that belong to one task.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TaskView {
    /// The task name: the smallest path slug among the members, otherwise
    /// the smallest branch name.
    pub slug: String,
    /// Members sorted by repository name, then path.
    pub members: Vec<TaskMember>,
}

/// Union-find over member indices, keeping the smallest index as root.
fn find(parent: &mut [usize], mut i: usize) -> usize {
    loop {
        let Some(&p) = parent.get(i) else {
            return i;
        };
        if p == i {
            return i;
        }
        let grand = parent.get(p).copied().unwrap_or(p);
        if let Some(slot) = parent.get_mut(i) {
            *slot = grand;
        }
        i = p;
    }
}

fn union(parent: &mut [usize], a: usize, b: usize) {
    let (ra, rb) = (find(parent, a), find(parent, b));
    let (lo, hi) = if ra < rb { (ra, rb) } else { (rb, ra) };
    if let Some(slot) = parent.get_mut(hi) {
        *slot = lo;
    }
}

/// Groups worktrees `(repository name, worktree)` into task views.
///
/// Two worktrees are in the same view when they share a key: the slug the
/// path pattern captures, or (with `by_branch`) the checked-out branch
/// name. Keys share one namespace, so a worktree at `.worktree/pay/api` and
/// another repository's worktree on branch `pay` are joined, and grouping
/// is transitive. Worktrees without any key (detached and outside the
/// pattern) are not part of any view; single-worktree views are returned.
/// Output is sorted by slug and deterministic.
pub fn group_task_views<'a>(
    worktrees: impl IntoIterator<Item = (&'a str, &'a WorktreeInfo)>,
    grouping: &TaskGrouping,
) -> Vec<TaskView> {
    struct Candidate<'a> {
        repo: &'a str,
        info: &'a WorktreeInfo,
        path_slug: Option<String>,
        branch: Option<&'a str>,
    }
    let mut members: Vec<Candidate<'a>> = worktrees
        .into_iter()
        .filter(|(_, info)| grouping.include_main || !info.is_main)
        .filter_map(|(repo, info)| {
            let path_slug = grouping
                .path_pattern
                .as_ref()
                .and_then(|p| p.slug_of(&info.path));
            let branch = if grouping.by_branch {
                info.branch.as_deref()
            } else {
                None
            };
            (path_slug.is_some() || branch.is_some()).then_some(Candidate {
                repo,
                info,
                path_slug,
                branch,
            })
        })
        .collect();
    members.sort_by(|a, b| (a.repo, &a.info.path).cmp(&(b.repo, &b.info.path)));

    let mut parent: Vec<usize> = (0..members.len()).collect();
    let mut first_with_key: BTreeMap<&str, usize> = BTreeMap::new();
    for (i, member) in members.iter().enumerate() {
        for key in member.path_slug.as_deref().into_iter().chain(member.branch) {
            match first_with_key.get(key) {
                Some(&j) => union(&mut parent, i, j),
                None => {
                    first_with_key.insert(key, i);
                }
            }
        }
    }

    let mut groups: BTreeMap<usize, Vec<usize>> = BTreeMap::new();
    for i in 0..members.len() {
        let root = find(&mut parent, i);
        groups.entry(root).or_default().push(i);
    }
    let mut views: Vec<TaskView> = groups
        .into_values()
        .filter_map(|indices| {
            let group: Vec<&Candidate<'a>> =
                indices.iter().filter_map(|&i| members.get(i)).collect();
            let slug = group
                .iter()
                .filter_map(|m| m.path_slug.as_deref())
                .min()
                .or_else(|| group.iter().filter_map(|m| m.branch).min())?
                .to_owned();
            Some(TaskView {
                slug,
                members: group
                    .iter()
                    .map(|m| TaskMember {
                        repo: m.repo.to_owned(),
                        worktree: m.info.clone(),
                    })
                    .collect(),
            })
        })
        .collect();
    views.sort_by(|a, b| {
        a.slug.cmp(&b.slug).then_with(|| {
            let first = |v: &TaskView| {
                v.members
                    .first()
                    .map(|m| (m.repo.clone(), m.worktree.path.clone()))
            };
            first(a).cmp(&first(b))
        })
    });
    views
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;

    fn wt(path: &str, branch: Option<&str>, is_main: bool) -> WorktreeInfo {
        WorktreeInfo {
            path: PathBuf::from(path),
            head_commit: Some("0".repeat(40)),
            branch: branch.map(str::to_owned),
            is_main,
            prunable: false,
        }
    }

    #[test]
    fn pattern_parsing() {
        assert!(PathPattern::new("{root}/.worktree/{slug}/{module}").is_ok());
        assert!(PathPattern::new(".worktree/{slug}/*").is_ok());
        assert!(PathPattern::new("{slug}").is_ok());
        assert_eq!(PathPattern::new(""), Err(PatternError::Empty));
        assert_eq!(PathPattern::new("{root}"), Err(PatternError::Empty));
        assert!(matches!(
            PathPattern::new(".worktree/*"),
            Err(PatternError::Slug(_))
        ));
        assert!(matches!(
            PathPattern::new("{slug}/{slug}"),
            Err(PatternError::Slug(_))
        ));
        for bad in [
            "a//{slug}",
            "../{slug}",
            "./{slug}",
            "{slug}/x{y}",
            "{slug}/a*b",
            "{other}/{slug}",
            "{slug}/",
            "a\\b/{slug}",
            "x/{root}/{slug}",
        ] {
            assert!(
                matches!(PathPattern::new(bad), Err(PatternError::Component(_))),
                "{bad}"
            );
        }
        assert_eq!(
            PathPattern::default(),
            PathPattern::new(".worktree/{slug}/*").unwrap()
        );
    }

    #[test]
    fn pattern_matches_path_suffix() {
        let p = PathPattern::new("{root}/.worktree/{slug}/{module}").unwrap();
        let path: PathBuf = ["work", "mono", ".worktree", "payment", "api"]
            .iter()
            .collect();
        assert_eq!(p.slug_of(&path).as_deref(), Some("payment"));
        let other: PathBuf = ["work", "mono", "worktrees", "payment", "api"]
            .iter()
            .collect();
        assert_eq!(p.slug_of(&other), None);
        assert_eq!(p.slug_of(Path::new("api")), None);
        assert_eq!(p.slug_of(Path::new("")), None);
    }

    #[cfg(unix)]
    #[test]
    fn non_utf8_paths_never_match() {
        use std::ffi::OsStr;
        use std::os::unix::ffi::OsStrExt;
        let p = PathPattern::default();
        let path = Path::new("/w/.worktree")
            .join(OsStr::from_bytes(b"x\xff"))
            .join("api");
        assert_eq!(p.slug_of(&path), None);
    }

    #[test]
    fn groups_by_path_and_branch_transitively() {
        let a1 = wt("/w/.worktree/pay/api", Some("feature/pay"), false);
        let b1 = wt("/w/.worktree/pay/web", Some("feature/pay-web"), false);
        // Different place, same branch as the api worktree: joined.
        let c1 = wt("/elsewhere/svc", Some("feature/pay"), false);
        // Unrelated task.
        let a2 = wt("/w/.worktree/search/api", None, false);
        // Main worktrees on a shared branch are not a task.
        let a_main = wt("/w/api", Some("development"), true);
        let b_main = wt("/w/web", Some("development"), true);
        // Detached and outside the pattern: no key, no view.
        let lone = wt("/tmp/x", None, false);
        let input = vec![
            ("web", &b1),
            ("api", &a1),
            ("svc", &c1),
            ("api", &a2),
            ("api", &a_main),
            ("web", &b_main),
            ("api", &lone),
        ];
        let views = group_task_views(input.clone(), &TaskGrouping::default());
        assert_eq!(views.len(), 2);
        assert_eq!(views[0].slug, "pay");
        let members: Vec<&str> = views[0].members.iter().map(|m| m.repo.as_str()).collect();
        assert_eq!(members, ["api", "svc", "web"]);
        assert_eq!(views[1].slug, "search");
        assert_eq!(views[1].members.len(), 1);

        // Deterministic regardless of input order.
        let mut reversed = input;
        reversed.reverse();
        assert_eq!(group_task_views(reversed, &TaskGrouping::default()), views);
    }

    #[test]
    fn grouping_switches() {
        let a = wt("/x/a", Some("topic"), false);
        let b = wt("/y/b", Some("topic"), false);
        let main = wt("/z/c", Some("topic"), true);
        let input = [("a", &a), ("b", &b), ("c", &main)];

        let by_branch = group_task_views(input, &TaskGrouping::default());
        assert_eq!(by_branch.len(), 1);
        assert_eq!(by_branch[0].slug, "topic");
        assert_eq!(by_branch[0].members.len(), 2);

        let with_main = TaskGrouping {
            include_main: true,
            ..TaskGrouping::default()
        };
        assert_eq!(group_task_views(input, &with_main)[0].members.len(), 3);

        let path_only = TaskGrouping {
            by_branch: false,
            ..TaskGrouping::default()
        };
        assert!(group_task_views(input, &path_only).is_empty());

        let nothing = TaskGrouping {
            path_pattern: None,
            by_branch: false,
            include_main: true,
        };
        assert!(group_task_views(input, &nothing).is_empty());
    }

    #[test]
    fn same_slug_from_path_and_branch_is_one_view() {
        let a = wt("/w/.worktree/pay/api", Some("feature/pay"), false);
        let b = wt("/other/web", Some("pay"), false);
        let views = group_task_views([("api", &a), ("web", &b)], &TaskGrouping::default());
        assert_eq!(views.len(), 1);
        assert_eq!(views[0].slug, "pay");
    }
}
