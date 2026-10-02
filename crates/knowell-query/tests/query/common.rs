//! Builders and fakes shared by the query tests.

use std::cell::Cell;
use std::collections::{BTreeMap, BTreeSet};

use knowell_core::{ContentHash, LineRange, Name, RepoPath};
use knowell_query::{
    Candidate, CommitId, EdgeKind, EvidenceType, ExactSource, ExactTarget, ExpandRequest,
    GraphExpander, GraphNode, Language, LexicalSource, Location, MatchDetail, Neighbor, PinnedView,
    ProjectCoverage, ProjectPin, QueryPlan, QueryScope, RerankItem, Reranker, Resolution,
    SearchConfig, Snippet, SnippetKind, SnippetRequest, SnippetSource, SourceError, SourceKind,
    SourceRequest, Tokenizer, VectorSource, ViewId, ViewManifest,
};

pub(crate) const COMMIT: &str = "0123456789abcdef0123456789abcdef01234567";

pub(crate) fn name(s: &str) -> Name {
    Name::new(s).unwrap()
}

pub(crate) fn view(s: &str) -> ViewId {
    ViewId::new(s).unwrap()
}

pub(crate) fn path(s: &str) -> RepoPath {
    RepoPath::new(s).unwrap()
}

pub(crate) fn lines(start: u32, end: u32) -> LineRange {
    LineRange::new(start, end).unwrap()
}

pub(crate) fn lang(s: &str) -> Language {
    Language::new(s).unwrap()
}

/// Content hash of a synthetic file version.
pub(crate) fn blob(s: &str) -> ContentHash {
    ContentHash::of(s.as_bytes())
}

pub(crate) fn pinned(view_name: &str, generation: u64) -> PinnedView {
    PinnedView {
        view: view(view_name),
        generation,
        commit: Some(CommitId::new(COMMIT).unwrap()),
    }
}

/// A project pinned at view `main`, generation 1, with Rust text and
/// reference resolution.
pub(crate) fn project_pin() -> ProjectPin {
    ProjectPin {
        base: pinned("main", 1),
        overlay: None,
        coverage: ProjectCoverage {
            languages: [lang("rust")].into(),
            reference_resolution: [lang("rust")].into(),
        },
    }
}

pub(crate) fn manifest(projects: &[&str]) -> ViewManifest {
    let mut manifest = ViewManifest::new(name("ws"));
    for project in projects {
        manifest.projects.insert(name(project), project_pin());
    }
    manifest
}

pub(crate) fn scope(projects: &[&str]) -> QueryScope {
    QueryScope::all(manifest(projects))
}

pub(crate) fn detail(source: SourceKind) -> MatchDetail {
    match source {
        SourceKind::Exact => MatchDetail::Exact {
            term: "term".into(),
            target: ExactTarget::Symbol,
        },
        SourceKind::Lexical => MatchDetail::Lexical {
            terms: vec!["term".into()],
        },
        SourceKind::Semantic => MatchDetail::Semantic {
            profile: "balanced-1536".into(),
        },
    }
}

/// A candidate in view `main`, generation 1, language Rust; its content hash
/// is derived from `project/file` so equal files in one project share it.
pub(crate) fn hit(
    source: SourceKind,
    rank: u32,
    project: &str,
    file: &str,
    range: (u32, u32),
) -> Candidate {
    Candidate {
        id: format!("{project}:{file}:{}-{}", range.0, range.1),
        project: name(project),
        view: view("main"),
        generation: 1,
        path: path(file),
        range: Some(lines(range.0, range.1)),
        content_hash: blob(&format!("{project}/{file}")),
        symbol: None,
        language: Some(lang("rust")),
        source,
        source_rank: rank,
        raw_score: 1.0 / f64::from(rank),
        detail: detail(source),
    }
}

pub(crate) fn location(project: &str, file: &str, range: (u32, u32)) -> Location {
    hit(SourceKind::Lexical, 1, project, file, range).location()
}

pub(crate) fn approx(a: f64, b: f64) -> bool {
    (a - b).abs() < 1e-12
}

/// A config with graph expansion off, for tests about fusion only.
pub(crate) fn no_expansion() -> SearchConfig {
    let mut config = SearchConfig::default();
    config.expansion.enabled = false;
    config
}

/// A candidate source that returns a fixed answer and records its calls.
pub(crate) struct FakeSource {
    answer: Result<Vec<Candidate>, SourceError>,
    pub(crate) calls: Cell<usize>,
    pub(crate) limits: Cell<Option<(usize, Option<usize>)>>,
}

impl FakeSource {
    pub(crate) fn answering(candidates: Vec<Candidate>) -> Self {
        Self {
            answer: Ok(candidates),
            calls: Cell::new(0),
            limits: Cell::new(None),
        }
    }

    pub(crate) fn failing(error: SourceError) -> Self {
        Self {
            answer: Err(error),
            calls: Cell::new(0),
            limits: Cell::new(None),
        }
    }

    fn respond(&self, request: &SourceRequest<'_>) -> Result<Vec<Candidate>, SourceError> {
        self.calls.set(self.calls.get() + 1);
        self.limits
            .set(Some((request.limit, request.per_project_limit)));
        self.answer.clone()
    }
}

impl ExactSource for FakeSource {
    fn search_exact(&self, request: &SourceRequest<'_>) -> Result<Vec<Candidate>, SourceError> {
        self.respond(request)
    }
}

impl LexicalSource for FakeSource {
    fn search_lexical(&self, request: &SourceRequest<'_>) -> Result<Vec<Candidate>, SourceError> {
        self.respond(request)
    }
}

impl VectorSource for FakeSource {
    fn search_semantic(&self, request: &SourceRequest<'_>) -> Result<Vec<Candidate>, SourceError> {
        self.respond(request)
    }
}

pub(crate) fn graph_node(project: &str, file: &str, range: (u32, u32), symbol: &str) -> GraphNode {
    GraphNode {
        location: location(project, file, range),
        symbol: Some(symbol.to_owned()),
        language: Some(lang("rust")),
    }
}

pub(crate) fn neighbor(
    node: GraphNode,
    edge: EdgeKind,
    evidence: EvidenceType,
    resolution: Resolution,
) -> Neighbor {
    Neighbor {
        node,
        edge,
        evidence,
        resolution,
    }
}

/// A graph keyed by node symbol (or location label when there is none).
#[derive(Default)]
pub(crate) struct FakeGraph {
    pub(crate) edges: BTreeMap<String, Vec<Neighbor>>,
    pub(crate) fail: Option<SourceError>,
    pub(crate) calls: Cell<usize>,
}

impl FakeGraph {
    pub(crate) fn add(&mut self, from: &str, to: Neighbor) {
        self.edges.entry(from.to_owned()).or_default().push(to);
    }
}

impl GraphExpander for FakeGraph {
    fn neighbors(&self, request: &ExpandRequest<'_>) -> Result<Vec<Neighbor>, SourceError> {
        self.calls.set(self.calls.get() + 1);
        if let Some(error) = &self.fail {
            return Err(error.clone());
        }
        let key = request
            .node
            .symbol
            .clone()
            .unwrap_or_else(|| request.node.location.label());
        Ok(self.edges.get(&key).cloned().unwrap_or_default())
    }
}

/// Snippets keyed by location label.
#[derive(Default)]
pub(crate) struct FakeSnippets {
    pub(crate) skeletons: BTreeMap<String, Snippet>,
    pub(crate) bodies: BTreeMap<String, Snippet>,
    pub(crate) failing: BTreeSet<String>,
}

impl FakeSnippets {
    pub(crate) fn set(&mut self, at: &Location, kind: SnippetKind, range: (u32, u32), text: &str) {
        let snippet = Snippet {
            text: text.to_owned(),
            range: lines(range.0, range.1),
            content_hash: at.content_hash,
        };
        let map = match kind {
            SnippetKind::Skeleton => &mut self.skeletons,
            SnippetKind::Body => &mut self.bodies,
        };
        map.insert(at.label(), snippet);
    }
}

impl SnippetSource for FakeSnippets {
    fn snippet(&self, request: &SnippetRequest<'_>) -> Result<Option<Snippet>, SourceError> {
        let key = request.location.label();
        if self.failing.contains(&key) {
            return Err(SourceError::Failed("disk read error".into()));
        }
        let map = match request.kind {
            SnippetKind::Skeleton => &self.skeletons,
            SnippetKind::Body => &self.bodies,
        };
        Ok(map.get(&key).cloned())
    }
}

/// One token per whitespace-separated word: makes budgets easy to compute
/// by hand. A citation label with a commit is exactly 4 words.
pub(crate) struct Words;

impl Tokenizer for Words {
    fn count_tokens(&self, text: &str) -> u32 {
        u32::try_from(text.split_whitespace().count()).unwrap()
    }
}

/// A reranker returning fixed scores.
pub(crate) struct FakeReranker {
    pub(crate) scores: Result<Vec<f64>, SourceError>,
    pub(crate) calls: Cell<usize>,
}

impl Reranker for FakeReranker {
    fn id(&self) -> String {
        "fake-reranker@1".into()
    }

    fn rerank(
        &self,
        _plan: &QueryPlan,
        _items: &[RerankItem<'_>],
    ) -> Result<Vec<f64>, SourceError> {
        self.calls.set(self.calls.get() + 1);
        self.scores.clone()
    }
}
