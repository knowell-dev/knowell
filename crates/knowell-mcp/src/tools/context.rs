//! `build_context` and `history`.

use knowell_core::{LineRange, Name, RepoPath};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use super::{
    Validate, check_len, check_limit, check_opt_text, check_range, check_text, check_texts,
    invalid, limits,
};
use crate::error::ToolError;
use crate::ids::{CommitId, JobId, MemoryId, ResultId, Timestamp};
use crate::model::{Evidence, FileLocator, Gap, JobRef, Target};
use crate::text::UntrustedText;
use crate::tools::memory::MemoryRecord;

// ---------------------------------------------------------------------------
// build_context
// ---------------------------------------------------------------------------

/// Input of `build_context`. Pass `task`, or `job_id` to collect a pending
/// pack.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct BuildContextInput {
    /// Context or workspace.
    #[serde(flatten)]
    pub target: Target,
    /// The task in your own words, e.g. 'add a retry limit to payment capture'.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(description = "The task in your words")]
    pub task: Option<String>,
    /// Token budget for the pack, in estimated tokens (default 8000).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(range(min = 256, max = 200000))]
    #[schemars(description = "default 8000")]
    pub token_budget: Option<u32>,
    /// Only these projects (all reachable projects when empty). This is a hard
    /// source filter, including named focus sources and relation expansion.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    #[schemars(description = "")]
    pub projects: Vec<Name>,
    /// Literal source-root-relative path prefixes, using the same conventions
    /// as `search`. A trailing `/` requires a directory boundary.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    #[schemars(description = "")]
    pub path_prefixes: Vec<String>,
    /// Only sources with one of these known languages (all when empty).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    #[schemars(description = "")]
    pub languages: Vec<String>,
    /// Files or ranges you already know are relevant. These are preferred
    /// sources, not a restriction on the rest of the pack.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    #[schemars(description = "Known-relevant files")]
    pub focus_paths: Vec<FileLocator>,
    /// Preferred symbols, by name, in-file qualified name or
    /// `path#qualified.name`. Ambiguous names remain explicit; these are not
    /// hard filters on the rest of the pack.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    #[schemars(description = "Known-relevant symbols")]
    pub focus_symbols: Vec<String>,
    /// Sections to include (all when empty).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    #[schemars(description = "")]
    pub include: Vec<ContextSection>,
    /// Pending pack to collect.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(description = "Collect a pending pack")]
    pub job_id: Option<JobId>,
    /// Source selection. Omitted uses complementary source packing.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(description = "default source; others are comparators")]
    pub selection_strategy: Option<ContextSelectionStrategy>,
    /// Revisable evidence needs for experimental selection, not a completeness claim.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    #[schemars(description = "")]
    pub desired_roles: Vec<ContextRole>,
}

impl Validate for BuildContextInput {
    fn validate(&self) -> Result<(), ToolError> {
        self.target.validate()?;
        super::code::SearchInput {
            target: self.target.clone(),
            query: "context scope".to_owned(),
            projects: self.projects.clone(),
            path_prefixes: self.path_prefixes.clone(),
            languages: self.languages.clone(),
            ..super::code::SearchInput::default()
        }
        .validate()?;
        for prefix in &self.path_prefixes {
            if prefix.chars().any(char::is_control)
                || RepoPath::new(prefix.trim().strip_suffix('/').unwrap_or(prefix.trim())).is_err()
            {
                return Err(invalid(
                    "`path_prefixes` must be relative, '/'-separated literal prefixes without empty, '.' or '..' components or control characters",
                ));
            }
        }
        match (&self.task, &self.job_id) {
            (None, None) => return Err(invalid("pass `task` or a `job_id`")),
            (Some(_), Some(_)) => {
                return Err(invalid("pass either `task` or `job_id`, not both"));
            }
            _ => {}
        }
        check_opt_text("task", self.task.as_deref(), limits::MAX_TASK_CHARS)?;
        check_range(
            "token_budget",
            self.token_budget,
            limits::MIN_TOKEN_BUDGET,
            limits::MAX_TOKEN_BUDGET,
        )?;
        check_len(
            "focus_paths",
            self.focus_paths.len(),
            limits::MAX_LIST_ITEMS,
        )?;
        for locator in &self.focus_paths {
            if !self.projects.is_empty() && !self.projects.contains(&locator.project) {
                return Err(invalid("`focus_paths` must be within `projects`"));
            }
            if !self.path_prefixes.is_empty()
                && !self
                    .path_prefixes
                    .iter()
                    .any(|prefix| locator.path.as_str().starts_with(prefix.trim()))
            {
                return Err(invalid("`focus_paths` must be within `path_prefixes`"));
            }
        }
        check_texts(
            "focus_symbols",
            &self.focus_symbols,
            limits::MAX_LIST_ITEMS,
            limits::MAX_SYMBOL_CHARS,
        )?;
        check_len("include", self.include.len(), limits::MAX_LIST_ITEMS)?;
        check_len(
            "desired_roles",
            self.desired_roles.len(),
            limits::MAX_LIST_ITEMS,
        )?;
        super::check_unique("desired_roles", &self.desired_roles)?;
        if self.selection_strategy.is_none() && !self.desired_roles.is_empty() {
            return Err(invalid("`desired_roles` requires `selection_strategy`"));
        }
        Ok(())
    }
}

/// Section of a context pack.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize, JsonSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum ContextSection {
    /// Code: signatures, skeletons and bodies.
    Code,
    /// Tests.
    Tests,
    /// Cross-project contracts.
    Contracts,
    /// Documentation.
    Docs,
    /// Accepted rules and approved examples.
    Rules,
    /// Decisions and findings.
    Memory,
}

/// Output of `build_context`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct BuildContextOutput {
    /// Pack entries, most important first.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub entries: Vec<ContextEntry>,
    /// Requested and used budget.
    pub budget: TokenBudget,
    /// Things the engine is unsure about; check them before relying on the pack.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub uncertainties: Vec<String>,
    /// Present while the pack is still being built.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub job: Option<JobRef>,
    /// What is missing from the pack, and why.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub gaps: Vec<Gap>,
    /// Experimental selection diagnostics and a source-backed inspection path.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub selection: Option<ContextSelectionReport>,
}

/// Source selection strategy. Comparator variants are explicit; none certifies
/// task completeness.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ContextSelectionStrategy {
    /// Adaptive complementary source packing without generated descriptions.
    #[default]
    #[schemars(description = "")]
    Source,
    /// Existing rank-ordered packing.
    #[schemars(description = "")]
    Rank,
    /// Relevance and diversity selection.
    #[schemars(description = "")]
    Mmr,
    /// Prefer additional supported evidence roles.
    #[schemars(description = "")]
    RoleCoverage,
    /// Evaluate bounded complementary source groups.
    #[schemars(description = "")]
    BoundedBundles,
}

/// A soft evidence need. A missing role remains unknown, not absent.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize, JsonSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum ContextRole {
    /// An authoritative entry-point location.
    #[schemars(description = "")]
    Entry,
    /// Implementation body.
    #[schemars(description = "")]
    Implementation,
    /// Supported incoming call evidence.
    #[schemars(description = "")]
    Caller,
    /// Supported outgoing call evidence.
    #[schemars(description = "")]
    Callee,
    /// Test evidence.
    #[schemars(description = "")]
    Test,
    /// Authoritative configuration dependency.
    #[schemars(description = "")]
    Config,
    /// Contract evidence.
    #[schemars(description = "")]
    Contract,
    /// Documentation.
    #[schemars(description = "")]
    Doc,
    /// Supported surrounding relations.
    #[schemars(description = "")]
    Surroundings,
}

/// Source-backed steps and bounded selection work for a context pack.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct ContextSelectionReport {
    /// Comparator used.
    pub strategy: ContextSelectionStrategy,
    /// Candidates considered by the selector.
    pub considered_candidates: u32,
    /// Candidates outside the selector's explicit cap.
    pub omitted_by_candidate_limit: u32,
    /// Source-set evaluations performed.
    pub evaluations: u32,
    /// Whether further set evaluation was prevented by the work limit.
    pub evaluation_budget_exhausted: bool,
    /// Selected source candidates.
    pub selected_candidates: u32,
    /// Roles supported by actual shown source bodies.
    pub covered_roles: Vec<ContextRole>,
    /// Requested roles without sufficient shown support.
    pub missing_roles: Vec<ContextRole>,
    /// Inspection steps pointing at actual fetchable entries.
    pub steps: Vec<ContextRoadmapStep>,
}

/// A supported part of the task's inspection path; the agent decides the action.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct ContextRoadmapStep {
    /// Evidence role supported by these entries.
    pub role: ContextRole,
    /// Fetchable ids of the actual shown sources supporting the role.
    pub source_ids: Vec<ResultId>,
}

/// Token budget accounting, in estimated tokens (a model-agnostic estimate,
/// roughly four bytes of text per token).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct TokenBudget {
    /// Budget requested (or the default).
    pub requested: u32,
    /// Estimated tokens used by the entries.
    pub used: u32,
}

/// One entry of a context pack.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct ContextEntry {
    /// Stable id; for entries with `evidence`, pass it to fetch for more.
    pub id: ResultId,
    /// Section.
    pub section: ContextSection,
    /// Form of the content.
    pub kind: EntryKind,
    /// Why this entry is relevant to the task.
    pub why_relevant: String,
    /// Source location, for code, tests, contracts and docs.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub evidence: Option<Evidence>,
    /// Memory record, for rules and decisions.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub memory_id: Option<MemoryId>,
    /// The content (untrusted).
    pub content: UntrustedText,
    /// Actual contiguous source lines in `content`, when known. Unlike
    /// `evidence.lines`, this describes what is displayed, not the fetch handle.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub content_lines: Option<knowell_core::LineRange>,
    /// The source body is a partial excerpt of the fetchable evidence range.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub content_truncated: bool,
    /// Exact pinned source ranges continuing a displayed excerpt. These are
    /// fetch handles, not inferred claims that a task has been covered.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub continuation_ids: Vec<ResultId>,
    /// Estimated tokens of `content`.
    pub estimated_tokens: u32,
}

/// Form of a context entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum EntryKind {
    /// A signature only.
    Signature,
    /// A skeleton (signatures of a module or class).
    Skeleton,
    /// Full code of a range.
    Code,
    /// A test.
    Test,
    /// A contract definition.
    Contract,
    /// A documentation section.
    Doc,
    /// An accepted rule or approved example.
    Rule,
    /// A decision or finding.
    Decision,
}

// ---------------------------------------------------------------------------
// history
// ---------------------------------------------------------------------------

/// Input of `history`. Pass `path` (optionally `lines`) or `symbol` within
/// `project`, or a result `id`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct HistoryInput {
    /// Context or workspace.
    #[serde(flatten)]
    pub target: Target,
    /// Project of `path` or `symbol`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(description = "Of path or symbol")]
    pub project: Option<Name>,
    /// File path.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(description = "")]
    pub path: Option<RepoPath>,
    /// Lines of `path`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(schema_with = "crate::model::lines_schema")]
    #[schemars(description = "Of path")]
    pub lines: Option<LineRange>,
    /// Symbol name, optionally qualified.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(description = "")]
    pub symbol: Option<String>,
    /// Result id instead of project + path/symbol.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(description = "Instead of project + path/symbol")]
    pub id: Option<ResultId>,
    /// Facets to include (all when empty).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    #[schemars(description = "")]
    pub include: Vec<HistoryFacet>,
    /// Maximum commits and co-changed files each (default 10).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(range(min = 1, max = 100))]
    #[schemars(description = "default 10")]
    pub limit: Option<u32>,
}

impl Validate for HistoryInput {
    fn validate(&self) -> Result<(), ToolError> {
        self.target.validate()?;
        let by_location = self.path.is_some() || self.symbol.is_some();
        match (&self.id, by_location) {
            (Some(_), true) => {
                return Err(invalid("pass either `id` or `path`/`symbol`, not both"));
            }
            (None, false) => return Err(invalid("pass `path`, `symbol` or `id`")),
            _ => {}
        }
        if self.path.is_some() && self.symbol.is_some() {
            return Err(invalid("pass either `path` or `symbol`, not both"));
        }
        if by_location && self.project.is_none() {
            return Err(invalid("`project` is required with `path` or `symbol`"));
        }
        if self.lines.is_some() && self.path.is_none() {
            return Err(invalid("`lines` only applies together with `path`"));
        }
        if let Some(symbol) = &self.symbol {
            check_text("symbol", symbol, limits::MAX_SYMBOL_CHARS)?;
        }
        check_len("include", self.include.len(), limits::MAX_LIST_ITEMS)?;
        check_limit(self.limit, 100)
    }
}

/// Parts of `history`.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize, JsonSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum HistoryFacet {
    /// Recent commits.
    Commits,
    /// Last commit per line range.
    Blame,
    /// Files that often change together with the subject.
    CoChanged,
    /// Recorded decisions and ADRs about the subject.
    Rationale,
}

/// Output of `history`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct HistoryOutput {
    /// Recent commits touching the subject, newest first.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub commits: Vec<CommitInfo>,
    /// Last change per line range.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub blame: Vec<BlameRange>,
    /// Files that change together with the subject, most frequent first.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub co_changed: Vec<CoChange>,
    /// Decisions and ADRs linked to the subject.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub rationale: Vec<MemoryRecord>,
    /// Why the result is empty or incomplete.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub gaps: Vec<Gap>,
}

/// A commit (author names only; e-mail addresses are never returned).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct CommitInfo {
    /// Commit id.
    pub commit: CommitId,
    /// Project.
    pub project: Name,
    /// Author name.
    pub author: String,
    /// Commit time.
    pub committed_at: Timestamp,
    /// First line of the message (untrusted).
    pub summary: UntrustedText,
    /// Number of files changed.
    pub files_changed: u32,
}

/// The last commit that changed a line range.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct BlameRange {
    /// Lines (1-based, inclusive) in the context's view.
    pub lines: LineRange,
    /// Commit that last changed them.
    pub commit: CommitId,
    /// Author name.
    pub author: String,
    /// Commit time.
    pub committed_at: Timestamp,
}

/// A file that changes together with the subject.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct CoChange {
    /// Project.
    pub project: Name,
    /// File path.
    pub path: RepoPath,
    /// Commits that changed both.
    pub together: u32,
    /// Commits that changed the subject (in the analysed window).
    pub of_commits: u32,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ids::ContextId;

    fn context_input() -> BuildContextInput {
        BuildContextInput {
            target: Target::context(ContextId::new("synthetic-context").unwrap()),
            task: Some("locate the format reader and mapping".to_owned()),
            ..BuildContextInput::default()
        }
    }

    #[test]
    fn context_hard_filters_use_literal_search_scope_conventions() {
        let mut input = context_input();
        input.projects = vec![Name::new("reader").unwrap()];
        input.path_prefixes = vec!["src/formats/".to_owned()];
        input.languages = vec!["rust".to_owned()];
        input.focus_paths = vec![FileLocator {
            project: Name::new("reader").unwrap(),
            path: RepoPath::new("src/formats/native.rs").unwrap(),
            lines: Some(LineRange::new(2, 7).unwrap()),
        }];
        assert!(input.validate().is_ok());
        let serialized = serde_json::to_value(&input).unwrap();
        let restored: BuildContextInput = serde_json::from_value(serialized).unwrap();
        assert_eq!(restored, input);

        input.projects.push(Name::new("reader").unwrap());
        assert!(input.validate().is_err());
    }

    #[test]
    fn context_focus_never_bypasses_hard_project_or_path_filters() {
        let mut input = context_input();
        input.projects = vec![Name::new("reader").unwrap()];
        input.path_prefixes = vec!["src/native/".to_owned()];
        input.focus_paths = vec![FileLocator {
            project: Name::new("unrelated").unwrap(),
            path: RepoPath::new("src/native/read.rs").unwrap(),
            lines: None,
        }];
        assert!(input.validate().is_err());
        input.focus_paths.first_mut().unwrap().project = Name::new("reader").unwrap();
        assert!(input.validate().is_ok());
        input.focus_paths.first_mut().unwrap().path = RepoPath::new("src/native_extra.rs").unwrap();
        assert!(input.validate().is_err());
    }

    #[test]
    fn malformed_context_prefixes_are_rejected_without_echoing_the_input() {
        for prefix in [
            "../outside",
            "src/../outside",
            "/absolute",
            "src\\native",
            "src/./native",
            "src//native",
            "src/\u{1b}native",
            "src/\0native",
        ] {
            let mut input = context_input();
            input.path_prefixes = vec![prefix.to_owned()];
            let error = input.validate().unwrap_err();
            assert!(!error.to_string().contains(prefix));
        }
        assert!(
            serde_json::from_str::<BuildContextInput>(
                r#"{"context_id":"synthetic-context","task":"reader","path_prefixes":["src/""#
            )
            .is_err()
        );
        assert!(
            serde_json::from_str::<BuildContextInput>(
                r#"{"context_id":"synthetic-context","task":"reader","projects":42}"#
            )
            .is_err()
        );
    }

    #[test]
    fn legacy_context_requests_leave_source_filters_unrestricted() {
        let input = context_input();
        assert!(input.validate().is_ok());
        let value = serde_json::to_value(input).unwrap();
        for field in ["projects", "path_prefixes", "languages"] {
            assert!(value.get(field).is_none());
        }
    }

    #[tokio::test]
    async fn fixture_context_applies_every_hard_source_filter() {
        use crate::tools::OpenWorkspaceInput;
        use crate::{Caller, FixtureTools, KnowellTools, TransportKind};

        let tools = FixtureTools::new();
        let caller = Caller::local(TransportKind::Stdio);
        let opened = tools
            .open_workspace(&caller, OpenWorkspaceInput::default())
            .await
            .unwrap();
        let mut input = BuildContextInput {
            target: Target::context(opened.context_id),
            task: Some("subscription cancellation".to_owned()),
            projects: vec![Name::new("billing-api").unwrap()],
            path_prefixes: vec!["src/payments/".to_owned()],
            languages: vec!["typescript".to_owned()],
            include: vec![
                ContextSection::Code,
                ContextSection::Tests,
                ContextSection::Contracts,
            ],
            ..BuildContextInput::default()
        };
        let context = tools.build_context(&caller, input.clone()).await.unwrap();
        assert!(!context.entries.is_empty());
        assert!(context.entries.iter().all(|entry| {
            entry.evidence.as_ref().is_some_and(|evidence| {
                evidence.project.as_str() == "billing-api"
                    && evidence.path.as_str().starts_with("src/payments/")
            })
        }));
        assert!(context.gaps.iter().all(|gap| {
            gap.project
                .as_ref()
                .is_none_or(|project| project.as_str() == "billing-api")
        }));
        input.languages = vec!["rust".to_owned()];
        let empty = tools.build_context(&caller, input).await.unwrap();
        assert!(empty.entries.is_empty());
        assert!(!empty.gaps.is_empty());
    }
}
