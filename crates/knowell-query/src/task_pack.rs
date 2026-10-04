//! Body-centered source selection and bounded research comparators.
//!
//! The objective is an uncalibrated research heuristic. A covered role means
//! that the selected bodies carry that kind of evidence, never that the task
//! is understood completely or that no relevant code was missed.

use std::cell::RefCell;
use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};

use crate::{
    CharsPerToken, Citation, ContextPack, EdgeKind, EvidenceType, ExactTarget, GraphStep, Language,
    Location, Omission, OmitReason, Origin, QueryError, Reason, SearchResponse, Snippet,
    SnippetKind, SnippetRequest, SnippetSource, SourceError, Tokenizer, pack_with,
};

mod source_pack;

/// Selection policy; named research comparators retain their original behavior.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskSelectionStrategy {
    /// Query-sensitive bodies, complementary source content and bounded work.
    #[default]
    Source,
    /// Existing skeleton-first packing, without candidate or evaluation caps.
    Rank,
    /// Rank relevance with a lexical metadata redundancy penalty.
    Mmr,
    /// Greedy rank relevance plus requested source-evidence roles.
    RoleCoverage,
    /// Role coverage and redundancy with bounded seed/support pair lookahead.
    BoundedBundles,
}

/// Soft evidence preferences, not a mandatory execution path or completeness proof.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EvidenceRole {
    /// A demonstrated program entry. Currently not inferred from names or paths.
    Entry,
    /// A retrieved named symbol body in a supported code language; relevance is inherited from search.
    Implementation,
    /// A caller and the body it calls, connected by an evidenced path.
    Caller,
    /// A callee and the body that calls it, connected by an evidenced path.
    Callee,
    /// A test and its subject, connected by an evidenced path.
    Test,
    /// Demonstrated configuration influence. Currently not inferred from names.
    Config,
    /// An exact contract match or a contract path with its source bodies.
    Contract,
    /// Documentation linked to its subject by an evidenced path.
    Doc,
    /// Type, import or structural surroundings; no call or test execution is asserted.
    Surroundings,
}

/// Bounds and soft preferences for context selection.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct TaskPackOptions {
    /// Source selection by default; explicit comparators remain available.
    pub strategy: TaskSelectionStrategy,
    /// Roles to reward if backed by emitted bodies; duplicates are ignored.
    pub desired_roles: Vec<EvidenceRole>,
    /// Maximum candidates considered by bounded policies (1-64).
    /// Results and expanded items alternate so neither lane consumes the cap first.
    pub candidate_limit: usize,
    /// Maximum source gain assessments or experimental trial packs (1-4096).
    pub evaluation_budget: usize,
}

impl Default for TaskPackOptions {
    fn default() -> Self {
        Self {
            strategy: TaskSelectionStrategy::Source,
            desired_roles: vec![
                EvidenceRole::Implementation,
                EvidenceRole::Caller,
                EvidenceRole::Callee,
                EvidenceRole::Test,
            ],
            candidate_limit: 32,
            evaluation_budget: 256,
        }
    }
}

impl TaskPackOptions {
    /// Validates explicit work bounds; no invalid setting is silently clamped.
    pub fn validate(&self) -> Result<(), QueryError> {
        validate_candidate_limit(self.candidate_limit)?;
        if !(1..=4096).contains(&self.evaluation_budget) {
            return Err(QueryError::InvalidConfig {
                field: "task_pack.evaluation_budget",
                reason: "must be between 1 and 4096",
            });
        }
        Ok(())
    }
}

fn validate_candidate_limit(candidate_limit: usize) -> Result<(), QueryError> {
    if !(1..=64).contains(&candidate_limit) {
        return Err(QueryError::InvalidConfig {
            field: "task_pack.candidate_limit",
            reason: "must be between 1 and 64",
        });
    }
    Ok(())
}

/// Returns the locations admitted to bounded task selection, without reading
/// bodies. `candidate_limit` is a count in 1–64; ranked and expanded lanes
/// alternate exactly as they do for [`TaskSelectionStrategy::Source`].
///
/// A location can still have a missing, stale or failed body after hydration.
/// Admission is not evidence that the source answers the query.
pub fn task_source_locations(
    response: &SearchResponse,
    candidate_limit: usize,
) -> Result<Vec<&Location>, QueryError> {
    validate_candidate_limit(candidate_limit)?;
    Ok(candidates(response)
        .into_iter()
        .take(candidate_limit)
        .map(|candidate| candidate.location)
        .collect())
}

/// Actual emitted bodies supporting a role, including path endpoints.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RoleEvidence {
    /// Evidence kind, not a claim that a task requirement is fully satisfied.
    pub role: EvidenceRole,
    /// Deduplicated citations of bodies actually present in this pack.
    pub citations: Vec<Citation>,
}

/// Work accounting and explicit evidence gaps for the selected context.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TaskSelectionReport {
    /// Requested comparator.
    pub strategy: TaskSelectionStrategy,
    /// Candidates admitted to the selector, including ones with unavailable snippets.
    pub considered_candidates: usize,
    /// Search-response candidates excluded by the selector's own shortlist cap.
    pub omitted_by_candidate_limit: usize,
    /// Source gain assessments or trial packs evaluated (one for rank).
    pub evaluations: usize,
    /// Work bound prevented another assessment; no optimality is claimed.
    pub evaluation_budget_exhausted: bool,
    /// Candidates represented by emitted text, including contained bodies.
    pub selected_candidates: usize,
    /// Requested roles backed by emitted bodies.
    pub covered_roles: Vec<EvidenceRole>,
    /// Requested roles without the required emitted body evidence.
    pub missing_roles: Vec<EvidenceRole>,
    /// Source citations for covered roles. Skeletons never certify body coverage.
    pub role_evidence: Vec<RoleEvidence>,
    /// Candidates in the shortlist absent from the final emitted representation.
    pub unselected: Vec<Location>,
    /// Observed unavailable, stale, failed or individually over-budget candidates.
    /// These are trial observations, separate from the final pack's omissions.
    pub candidate_issues: Vec<Omission>,
}

/// Existing context representation plus opt-in selection diagnostics.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct TaskContextPack {
    /// Packed source text, with the existing budget, overlap and freshness checks.
    pub pack: ContextPack,
    /// What the bounded selector considered and which requested evidence is missing.
    pub selection: TaskSelectionReport,
}

/// Selects task evidence using the default approximate tokenizer.
pub fn pack_task(
    response: &SearchResponse,
    budget_tokens: u32,
    snippets: &dyn SnippetSource,
    options: &TaskPackOptions,
) -> Result<TaskContextPack, QueryError> {
    pack_task_with(
        response,
        budget_tokens,
        snippets,
        &CharsPerToken::default(),
        options,
    )
}

/// Selects source bodies or compares evidence sets through the legacy packer.
///
/// Source fetches each admitted body once, scores its actual source content,
/// removes same-file overlap and selects positive marginal value per cost.
/// Rank delegates directly to [`pack_with`]. Research policies maximize
/// an explicitly heuristic objective over emitted representations: reciprocal
/// square-root rank relevance, optional metadata-token redundancy, and optional
/// requested role coverage. `bounded_bundles` also tries seed/support pairs.
/// Snippets are cached per exact request for this call; no LLM or model API runs.
/// All original search uncertainties survive selection, including weak paths
/// outside the chosen set. Bodies, relationships and missing roles remain
/// inspectable; the selector does not certify sufficiency or exhaustive recall.
pub fn pack_task_with(
    response: &SearchResponse,
    budget_tokens: u32,
    snippets: &dyn SnippetSource,
    tokenizer: &dyn Tokenizer,
    options: &TaskPackOptions,
) -> Result<TaskContextPack, QueryError> {
    options.validate()?;
    let all = candidates(response);
    let requested: BTreeSet<_> = options.desired_roles.iter().copied().collect();
    if options.strategy == TaskSelectionStrategy::Rank {
        let pack = pack_with(response, budget_tokens, snippets, tokenizer);
        let selection = report(
            &pack,
            &all,
            &requested,
            options.strategy,
            0,
            1,
            false,
            vec![],
        );
        return Ok(TaskContextPack { pack, selection });
    }

    let omitted = all.len().saturating_sub(options.candidate_limit);
    let shortlist: Vec<_> = all.into_iter().take(options.candidate_limit).collect();
    if options.strategy == TaskSelectionStrategy::Source {
        let outcome = source_pack::pack(
            response,
            budget_tokens,
            snippets,
            tokenizer,
            options,
            &shortlist,
        );
        let selection = report(
            &outcome.pack,
            &shortlist,
            &requested,
            options.strategy,
            omitted,
            outcome.evaluations,
            outcome.exhausted,
            outcome.pack.omitted.clone(),
        );
        return Ok(TaskContextPack {
            pack: outcome.pack,
            selection,
        });
    }
    let cached = CachedSnippets {
        source: snippets,
        values: RefCell::new(BTreeMap::new()),
    };
    let mut trial = TrialRunner {
        response,
        candidates: &shortlist,
        budget_tokens,
        snippets: &cached,
        tokenizer,
        strategy: options.strategy,
        requested: &requested,
        remaining: options.evaluation_budget,
        evaluations: 0,
        exhausted: false,
        issues: Vec::new(),
    };
    let mut chosen = BTreeSet::new();
    let mut best_pack = empty_pack(response, budget_tokens);
    let mut best_score = 0.0;

    loop {
        let mut improvement: Option<(BTreeSet<usize>, ContextPack, f64)> = None;
        for index in 0..shortlist.len() {
            if chosen.contains(&index) {
                continue;
            }
            let mut next = chosen.clone();
            next.insert(index);
            let Some((pack, score)) = trial.evaluate(&next) else {
                break;
            };
            consider(&mut improvement, next, pack, score, best_score);
            if options.strategy != TaskSelectionStrategy::BoundedBundles {
                continue;
            }
            // A linked pair can have value that neither body provides alone.
            let Some(candidate) = shortlist.get(index) else {
                continue;
            };
            for peer in candidate.peers(&shortlist) {
                if chosen.contains(&peer) || peer == index {
                    continue;
                }
                let mut next = chosen.clone();
                next.insert(index);
                next.insert(peer);
                let Some((pack, score)) = trial.evaluate(&next) else {
                    break;
                };
                consider(&mut improvement, next, pack, score, best_score);
            }
            if trial.exhausted {
                break;
            }
        }
        let Some((next, pack, score)) = improvement else {
            break;
        };
        chosen = next;
        best_pack = pack;
        best_score = score;
        if trial.exhausted || chosen.len() == shortlist.len() {
            break;
        }
    }
    // Selection must not erase weak or missing evidence reported by search.
    best_pack.uncertainties = crate::pack::uncertainties(response);
    let selection = report(
        &best_pack,
        &shortlist,
        &requested,
        options.strategy,
        omitted,
        trial.evaluations,
        trial.exhausted,
        trial.issues,
    );
    Ok(TaskContextPack {
        pack: best_pack,
        selection,
    })
}

type SnippetKey = (Location, Option<String>, SnippetKind);
type SnippetValue = Result<Option<Snippet>, SourceError>;

struct CachedSnippets<'a> {
    source: &'a dyn SnippetSource,
    values: RefCell<BTreeMap<SnippetKey, SnippetValue>>,
}

impl SnippetSource for CachedSnippets<'_> {
    fn snippet(&self, request: &SnippetRequest<'_>) -> SnippetValue {
        let key = (
            request.location.clone(),
            request.symbol.map(str::to_owned),
            request.kind,
        );
        if let Some(value) = self.values.borrow().get(&key) {
            return value.clone();
        }
        let value = self.source.snippet(request);
        self.values.borrow_mut().insert(key, value.clone());
        value
    }
}

struct Candidate<'a> {
    source: CandidateSource,
    location: &'a Location,
    commit: Option<&'a crate::CommitId>,
    origin: Origin,
    relevance: f64,
    roles: BTreeSet<EvidenceRole>,
    paths: Vec<(&'a Location, &'a [GraphStep])>,
    terms: BTreeSet<String>,
}

#[derive(Clone, Copy)]
enum CandidateSource {
    Result(usize),
    Expanded(usize),
}

impl Candidate<'_> {
    fn peers(&self, all: &[Candidate<'_>]) -> BTreeSet<usize> {
        all.iter()
            .enumerate()
            .filter_map(|(index, other)| {
                let forward = self.paths.iter().any(|(seed, _)| *seed == other.location);
                let reverse = other.paths.iter().any(|(seed, _)| *seed == self.location);
                (forward || reverse).then_some(index)
            })
            .collect()
    }
}

fn candidates(response: &SearchResponse) -> Vec<Candidate<'_>> {
    let mut out = Vec::new();
    // Alternation reserves admission for independently ranked results and
    // graph context. It does not treat either lane as an exhaustive boundary.
    for index in 0..response.results.len().max(response.expanded.len()) {
        if let Some(result) = response.results.get(index) {
            let mut roles = BTreeSet::new();
            if result
                .symbol
                .as_ref()
                .is_some_and(|symbol| !symbol.trim().is_empty())
                && result.location.range.is_some()
                && result.language.as_ref().is_some_and(is_code_language)
            {
                roles.insert(EvidenceRole::Implementation);
            }
            if result.why.iter().any(|reason| {
                matches!(
                    reason,
                    Reason::ExactMatch {
                        target: ExactTarget::Contract,
                        ..
                    }
                )
            }) {
                roles.insert(EvidenceRole::Contract);
            }
            out.push(Candidate {
                source: CandidateSource::Result(index),
                location: &result.location,
                commit: result.commit.as_ref(),
                origin: Origin::Result { rank: result.rank },
                relevance: 1.0 / f64::from(result.rank.max(1)).sqrt(),
                roles,
                paths: paths(&result.why),
                terms: metadata_terms(&result.location, result.symbol.as_deref(), &result.why),
            });
        }
        if let Some(item) = response.expanded.get(index) {
            let mut related = paths(&item.why);
            if related.is_empty()
                && let Some(seed) = response.results.iter().find(|r| r.rank == item.seed_rank)
            {
                related.push((&seed.location, item.path.as_slice()));
            }
            out.push(Candidate {
                source: CandidateSource::Expanded(index),
                location: &item.location,
                commit: item.commit.as_ref(),
                origin: Origin::Expanded {
                    seed_rank: item.seed_rank,
                    depth: item.depth,
                },
                relevance: 0.4
                    / f64::from(item.seed_rank.max(1)).sqrt()
                    / f64::from(item.depth.max(1)),
                roles: BTreeSet::new(),
                paths: related,
                terms: metadata_terms(&item.location, item.symbol.as_deref(), &item.why),
            });
        }
    }
    out
}

fn paths(reasons: &[Reason]) -> Vec<(&Location, &[GraphStep])> {
    reasons
        .iter()
        .filter_map(|reason| match reason {
            Reason::GraphPath { seed, steps } => Some((seed, steps.as_slice())),
            _ => None,
        })
        .collect()
}

fn is_code_language(language: &Language) -> bool {
    // Query results do not yet carry symbol kind. A named Markdown heading,
    // schema key or unknown-language region must not stand in for code.
    matches!(
        language.as_str(),
        "rust"
            | "typescript"
            | "tsx"
            | "javascript"
            | "jsx"
            | "python"
            | "go"
            | "java"
            | "kotlin"
            | "csharp"
            | "c#"
            | "dart"
            | "swift"
            | "php"
            | "ruby"
            | "c"
            | "cpp"
            | "c++"
            | "scala"
            | "bash"
            | "lua"
            | "perl"
            | "r"
            | "elixir"
            | "erlang"
            | "haskell"
            | "ocaml"
            | "clojure"
            | "zig"
            | "objc"
            | "powershell"
            | "batch"
            | "groovy"
    )
}

fn metadata_terms(
    location: &Location,
    symbol: Option<&str>,
    reasons: &[Reason],
) -> BTreeSet<String> {
    let mut texts = vec![location.path.as_str()];
    if let Some(symbol) = symbol {
        texts.push(symbol);
    }
    for reason in reasons {
        if let Reason::LexicalTerms { terms } = reason {
            texts.extend(terms.iter().map(String::as_str));
        }
    }
    texts
        .into_iter()
        .flat_map(|text| text.split(|c: char| !c.is_alphanumeric()))
        .filter(|term| term.chars().count() > 1)
        .take(256)
        .map(crate::text::fold)
        .collect()
}

fn empty_pack(response: &SearchResponse, budget_tokens: u32) -> ContextPack {
    ContextPack {
        budget_tokens,
        used_tokens: 0,
        items: Vec::new(),
        omitted: Vec::new(),
        uncertainties: crate::pack::uncertainties(response),
    }
}

struct TrialRunner<'a, 'b> {
    response: &'a SearchResponse,
    candidates: &'b [Candidate<'a>],
    budget_tokens: u32,
    snippets: &'b dyn SnippetSource,
    tokenizer: &'b dyn Tokenizer,
    strategy: TaskSelectionStrategy,
    requested: &'b BTreeSet<EvidenceRole>,
    remaining: usize,
    evaluations: usize,
    exhausted: bool,
    issues: Vec<Omission>,
}

impl TrialRunner<'_, '_> {
    fn evaluate(&mut self, indices: &BTreeSet<usize>) -> Option<(ContextPack, f64)> {
        if self.remaining == 0 {
            self.exhausted = true;
            return None;
        }
        self.remaining -= 1;
        self.evaluations = self.evaluations.saturating_add(1);
        let mut response = SearchResponse {
            plan: self.response.plan.clone(),
            results: Vec::new(),
            expanded: Vec::new(),
            degraded: self.response.degraded.clone(),
            searched: self.response.searched.clone(),
            coverage_gaps: self.response.coverage_gaps.clone(),
            empty: self.response.empty.clone(),
            stats: self.response.stats.clone(),
        };
        for candidate in indices
            .iter()
            .filter_map(|index| self.candidates.get(*index))
        {
            match candidate.source {
                CandidateSource::Result(index) => {
                    if let Some(result) = self.response.results.get(index) {
                        response.results.push(result.clone());
                    }
                }
                CandidateSource::Expanded(index) => {
                    if let Some(item) = self.response.expanded.get(index) {
                        response.expanded.push(item.clone());
                    }
                }
            }
        }
        let pack = pack_with(&response, self.budget_tokens, self.snippets, self.tokenizer);
        for issue in &pack.omitted {
            let unavailable = matches!(
                issue.reason,
                OmitReason::StaleContent { .. }
                    | OmitReason::NoSnippet
                    | OmitReason::SnippetFailed { .. }
            );
            if (unavailable || indices.len() == 1) && !self.issues.contains(issue) {
                self.issues.push(issue.clone());
            }
        }
        let score = objective(&pack, self.candidates, self.requested, self.strategy);
        Some((pack, score))
    }
}

fn consider(
    best: &mut Option<(BTreeSet<usize>, ContextPack, f64)>,
    indices: BTreeSet<usize>,
    pack: ContextPack,
    score: f64,
    current_score: f64,
) {
    if score <= current_score {
        return;
    }
    let better = best.as_ref().is_none_or(|(previous, old_pack, old_score)| {
        score.total_cmp(old_score).is_gt()
            || (score.total_cmp(old_score).is_eq()
                && (pack.used_tokens, &indices) < (old_pack.used_tokens, previous))
    });
    if better {
        *best = Some((indices, pack, score));
    }
}

fn matches_citation(location: &Location, citation: &Citation) -> bool {
    location.project == citation.project
        && location.view == citation.view
        && location.generation == citation.generation
        && location.path == citation.path
        && location.content_hash == citation.content_hash
        && location.range.is_none_or(|range| {
            citation.range.start() <= range.start() && range.end() <= citation.range.end()
        })
}

fn body<'a>(pack: &'a ContextPack, location: &Location) -> Option<&'a Citation> {
    pack.items
        .iter()
        .find(|item| item.kind == SnippetKind::Body && matches_citation(location, &item.citation))
        .map(|item| &item.citation)
}

fn candidate_body<'a>(pack: &'a ContextPack, candidate: &Candidate<'_>) -> Option<&'a Citation> {
    pack.items
        .iter()
        .find(|item| {
            item.kind == SnippetKind::Body
                && item.citation.commit.as_ref() == candidate.commit
                && matches_citation(candidate.location, &item.citation)
        })
        .map(|item| &item.citation)
}

fn represented(pack: &ContextPack, candidate: &Candidate<'_>) -> bool {
    candidate_body(pack, candidate).is_some()
        || pack.items.iter().any(|item| {
            item.origin == candidate.origin
                && item.citation.project == candidate.location.project
                && item.citation.view == candidate.location.view
                && item.citation.generation == candidate.location.generation
                && item.citation.path == candidate.location.path
                && item.citation.content_hash == candidate.location.content_hash
                && item.citation.commit.as_ref() == candidate.commit
                && candidate
                    .location
                    .range
                    .is_none_or(|range| item.citation.range.overlaps(&range))
        })
}

fn role_for_edge(edge: EdgeKind) -> EvidenceRole {
    match edge {
        EdgeKind::Caller => EvidenceRole::Caller,
        EdgeKind::Callee => EvidenceRole::Callee,
        EdgeKind::Type => EvidenceRole::Surroundings,
        EdgeKind::Test => EvidenceRole::Test,
        EdgeKind::Contract => EvidenceRole::Contract,
        EdgeKind::Doc => EvidenceRole::Doc,
    }
}

fn role_for_path(seed_is_code_symbol: bool, steps: &[GraphStep]) -> Option<EvidenceRole> {
    let last = steps.last()?;
    let mut structural = false;
    let mut from_is_symbol = seed_is_code_symbol;
    for step in steps {
        let to_is_symbol = step.to.range.is_some()
            && step
                .symbol
                .as_ref()
                .is_some_and(|symbol| !symbol.trim().is_empty());
        if matches!(
            step.edge,
            EdgeKind::Caller | EdgeKind::Callee | EdgeKind::Test
        ) && (!from_is_symbol
            || !to_is_symbol
            || !matches!(
                step.evidence,
                EvidenceType::SemanticallyResolved | EvidenceType::RuntimeObservation
            ))
        {
            structural = true;
        }
        from_is_symbol = to_is_symbol;
    }
    // Adapters historically expose file imports through caller/callee/test
    // lanes. Resolved syntax and co-emitted bodies do not establish a call
    // or that a test exercises the subject. Preserve them as surroundings.
    Some(if structural {
        EvidenceRole::Surroundings
    } else {
        role_for_edge(last.edge)
    })
}

fn evidence(
    pack: &ContextPack,
    candidates: &[Candidate<'_>],
) -> BTreeMap<EvidenceRole, BTreeSet<Citation>> {
    let mut roles: BTreeMap<EvidenceRole, BTreeSet<Citation>> = BTreeMap::new();
    for candidate in candidates {
        let Some(citation) = candidate_body(pack, candidate) else {
            continue;
        };
        for role in &candidate.roles {
            roles.entry(*role).or_default().insert(citation.clone());
        }
        for (seed, steps) in &candidate.paths {
            let Some(last) = steps.last() else {
                continue;
            };
            if last.to != *candidate.location || steps.iter().any(GraphStep::is_uncertain) {
                continue;
            }
            let Some(seed_body) = body(pack, seed) else {
                continue;
            };
            let Some(mut citations) = steps
                .iter()
                .map(|step| body(pack, &step.to).cloned())
                .collect::<Option<BTreeSet<_>>>()
            else {
                continue;
            };
            citations.insert(seed_body.clone());
            let seed_is_code_symbol = candidates.iter().any(|candidate| {
                candidate.location == *seed
                    && candidate.roles.contains(&EvidenceRole::Implementation)
            });
            if let Some(role) = role_for_path(seed_is_code_symbol, steps) {
                roles.entry(role).or_default().extend(citations);
            }
        }
    }
    roles
}

fn objective(
    pack: &ContextPack,
    candidates: &[Candidate<'_>],
    requested: &BTreeSet<EvidenceRole>,
    strategy: TaskSelectionStrategy,
) -> f64 {
    let present: Vec<_> = candidates
        .iter()
        .filter(|candidate| represented(pack, candidate))
        .collect();
    let relevance: f64 = present
        .iter()
        .map(|candidate| {
            candidate.relevance
                * if candidate_body(pack, candidate).is_some() {
                    1.0
                } else {
                    0.15
                }
        })
        .sum();
    let roles = if matches!(
        strategy,
        TaskSelectionStrategy::RoleCoverage | TaskSelectionStrategy::BoundedBundles
    ) {
        let count = evidence(pack, candidates)
            .keys()
            .filter(|role| requested.contains(role))
            .count();
        1.5 * f64::from(u32::try_from(count).unwrap_or(u32::MAX))
    } else {
        0.0
    };
    let mut redundancy = 0.0;
    if matches!(
        strategy,
        TaskSelectionStrategy::Mmr | TaskSelectionStrategy::BoundedBundles
    ) {
        for (index, candidate) in present.iter().enumerate() {
            let penalty = present
                .iter()
                .take(index)
                .map(|previous| {
                    let intersection = candidate.terms.intersection(&previous.terms).count();
                    let union = candidate.terms.union(&previous.terms).count().max(1);
                    f64::from(u32::try_from(intersection).unwrap_or(u32::MAX))
                        / f64::from(u32::try_from(union).unwrap_or(u32::MAX))
                })
                .fold(0.0_f64, f64::max);
            redundancy += 0.7 * candidate.relevance * penalty;
        }
    }
    relevance + roles - redundancy
}

#[allow(clippy::too_many_arguments)]
fn report(
    pack: &ContextPack,
    candidates: &[Candidate<'_>],
    requested: &BTreeSet<EvidenceRole>,
    strategy: TaskSelectionStrategy,
    omitted: usize,
    evaluations: usize,
    exhausted: bool,
    candidate_issues: Vec<Omission>,
) -> TaskSelectionReport {
    let roles = evidence(pack, candidates);
    let covered_roles: Vec<_> = requested
        .iter()
        .filter(|role| roles.contains_key(role))
        .copied()
        .collect();
    let missing_roles = requested
        .iter()
        .filter(|role| !roles.contains_key(role))
        .copied()
        .collect();
    let role_evidence = covered_roles
        .iter()
        .filter_map(|role| {
            roles.get(role).map(|citations| RoleEvidence {
                role: *role,
                citations: citations.iter().cloned().collect(),
            })
        })
        .collect();
    let unselected: Vec<_> = candidates
        .iter()
        .filter(|candidate| !represented(pack, candidate))
        .map(|candidate| candidate.location.clone())
        .collect();
    TaskSelectionReport {
        strategy,
        considered_candidates: candidates.len(),
        omitted_by_candidate_limit: omitted,
        evaluations,
        evaluation_budget_exhausted: exhausted,
        selected_candidates: candidates.len().saturating_sub(unselected.len()),
        covered_roles,
        missing_roles,
        role_evidence,
        unselected,
        candidate_issues,
    }
}
