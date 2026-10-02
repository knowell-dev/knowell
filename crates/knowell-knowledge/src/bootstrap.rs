//! Session bootstrap: the budgeted, sourced pack a fresh session starts from.

use knowell_core::Name;
use knowell_secrets::redact;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::conflict::{Conflict, detect_conflicts};
use crate::ids::{RecordId, TaskId};
use crate::model::{Evidence, KnowledgeRecord, RecordKind, RecordState, Scope};
use crate::task::Task;

/// Below this many tokens of room, a too-big item is omitted instead of cut.
const MIN_PARTIAL_TOKENS: usize = 24;
const TRUNCATION_MARKER: &str = "\n[...truncated to fit the token budget]";

/// Priority class of a bootstrap item; earlier variants rank first.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize, JsonSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum Tier {
    /// Pinned accepted records.
    Pinned,
    /// Accepted records tagged `rule`.
    Rule,
    /// Accepted human-authored records that are not rules (ADRs, decisions); newest first.
    Decision,
    /// Tasks that are not finished; most recently updated first.
    OpenTask,
    /// Per-project maps.
    ProjectMap,
    /// Remaining accepted records (observed facts, accepted suggestions). Not
    /// in the required ranking; included last so they are not lost.
    Knowledge,
}

/// Overview of one project (roles, layout, how to run it), produced by deterministic analysis.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct ProjectMap {
    /// The project.
    pub project: Name,
    /// Markdown summary. Passed through the secret redactor before use.
    pub summary: String,
    /// Where the summary comes from.
    pub sources: Vec<Evidence>,
}

/// Everything the pack is built from. The caller selects what the session
/// may see; this crate does not know about permissions, so user-scoped or
/// foreign task records must be filtered out by the caller (or via
/// [`BootstrapInputs::scope_filter`]).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct BootstrapInputs {
    /// Candidate records. Only accepted ones are used.
    pub records: Vec<KnowledgeRecord>,
    /// Candidate tasks. Only unfinished ones are used.
    pub tasks: Vec<Task>,
    /// Project maps.
    pub project_maps: Vec<ProjectMap>,
    /// When set, only records whose scope equals one of these are used.
    pub scope_filter: Option<Vec<Scope>>,
}

/// Where an item comes from.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct ItemSource {
    /// The record, if the item is a record.
    pub record: Option<RecordId>,
    /// The task, if the item is a task.
    pub task: Option<TaskId>,
    /// The project, if the item is a project map.
    pub project: Option<Name>,
    /// Evidence backing the item.
    pub evidence: Vec<Evidence>,
}

/// One entry of the pack.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct BootstrapItem {
    /// Priority class.
    pub tier: Tier,
    /// Title.
    pub title: String,
    /// Rendered Markdown text (possibly truncated).
    pub text: String,
    /// Where it comes from.
    pub source: ItemSource,
    /// Estimated tokens of `text` (characters / 4, rounded up).
    pub tokens: usize,
    /// Whether `text` was cut to fit.
    pub truncated: bool,
}

/// An item that did not fit.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct OmittedItem {
    /// Priority class.
    pub tier: Tier,
    /// Title.
    pub title: String,
    /// Where it comes from.
    pub source: ItemSource,
    /// Estimated tokens it would have needed.
    pub tokens: usize,
}

/// The result of [`bootstrap_pack`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct BootstrapPack {
    /// The budget it was built for, in estimated tokens.
    pub budget_tokens: usize,
    /// Estimated tokens used by `items`.
    pub used_tokens: usize,
    /// Included items, in priority order.
    pub items: Vec<BootstrapItem>,
    /// Items that did not fit, in priority order. Nothing is dropped silently.
    pub omitted: Vec<OmittedItem>,
    /// Number of stale records excluded because they are not current.
    pub stale_excluded: usize,
    /// Conflicts among the accepted records considered. Not budgeted: a
    /// contradiction must never be hidden by truncation.
    pub conflicts: Vec<Conflict>,
}

/// Estimated tokens of `text`: characters divided by four, rounded up.
pub fn estimate_tokens(text: &str) -> usize {
    text.chars().count().div_ceil(4)
}

struct Candidate {
    tier: Tier,
    title: String,
    text: String,
    source: ItemSource,
}

fn record_candidate(tier: Tier, r: &KnowledgeRecord) -> Candidate {
    let mut text = format!(
        "### {}\n`{}` - {} - scope {} - v{}\n\n{}",
        r.title,
        r.subject,
        r.kind,
        r.scope.key(),
        r.version,
        r.body.trim()
    );
    if !r.evidence.is_empty() {
        text.push_str("\n\nSources:");
        for ev in &r.evidence {
            text.push_str(&format!(
                "\n- {}:{}#L{}-L{} @{}",
                ev.project,
                ev.path,
                ev.range.start(),
                ev.range.end(),
                ev.commit
            ));
        }
    }
    Candidate {
        tier,
        title: r.title.clone(),
        text,
        source: ItemSource {
            record: Some(r.id),
            task: None,
            project: None,
            evidence: r.evidence.clone(),
        },
    }
}

fn task_candidate(t: &Task) -> Candidate {
    let mut text = format!(
        "### Task: {}\nstatus: {} - id {}\n\nGoal: {}",
        t.title, t.status, t.id, t.goal
    );
    if let Some(note) = t.notes.last() {
        text.push_str(&format!("\n\nLatest note: {}", note.text));
    }
    let open: Vec<&str> = t
        .open_questions
        .iter()
        .filter(|q| q.resolved_at.is_none())
        .map(|q| q.text.as_str())
        .collect();
    if !open.is_empty() {
        text.push_str("\n\nOpen questions:");
        for q in open {
            text.push_str(&format!("\n- {q}"));
        }
    }
    Candidate {
        tier: Tier::OpenTask,
        title: t.title.clone(),
        text,
        source: ItemSource {
            record: None,
            task: Some(t.id),
            project: None,
            evidence: Vec::new(),
        },
    }
}

fn map_candidate(m: &ProjectMap) -> Candidate {
    Candidate {
        tier: Tier::ProjectMap,
        title: format!("Project map: {}", m.project),
        text: format!(
            "### Project map: {}\n\n{}",
            m.project,
            redact(m.summary.trim()).text
        ),
        source: ItemSource {
            record: None,
            task: None,
            project: Some(m.project.clone()),
            evidence: m.sources.clone(),
        },
    }
}

/// Cuts `text` to at most `max_tokens` estimated tokens, ending with a marker.
fn truncate_to(text: &str, max_tokens: usize) -> Option<String> {
    let max_chars = max_tokens
        .checked_mul(4)?
        .checked_sub(TRUNCATION_MARKER.chars().count())?;
    if max_chars == 0 {
        return None;
    }
    let mut cut: String = text.chars().take(max_chars).collect();
    cut.push_str(TRUNCATION_MARKER);
    Some(cut)
}

/// Builds the bootstrap pack for a session.
///
/// Ranking (earlier first): pinned records, accepted rules (tag `rule`),
/// recent accepted decisions (human-authored, newest first), open tasks,
/// project maps, then other accepted knowledge. Within a tier the order is
/// explicit and total: broader scope first for pinned records and rules, then
/// subject, then id; newest first for decisions and tasks, then id; project
/// name for maps. A record appears once, in its highest tier.
///
/// Only **accepted** records are used; stale records are excluded and
/// counted in [`BootstrapPack::stale_excluded`].
///
/// Budget: items are taken in order while they fit (size = characters / 4).
/// An item that does not fit is cut to the remaining room if at least 24
/// tokens remain (it is flagged `truncated`, and the budget is then spent);
/// otherwise it is omitted and smaller later items may still fit. Every
/// omitted item is listed. The output depends only on the inputs.
pub fn bootstrap_pack(inputs: &BootstrapInputs, budget_tokens: usize) -> BootstrapPack {
    let in_scope = |r: &&KnowledgeRecord| {
        inputs
            .scope_filter
            .as_ref()
            .is_none_or(|f| f.contains(&r.scope))
    };
    let visible: Vec<&KnowledgeRecord> = inputs.records.iter().filter(in_scope).collect();
    let stale_excluded = visible
        .iter()
        .filter(|r| r.state == RecordState::Stale)
        .count();
    let accepted: Vec<&KnowledgeRecord> = visible
        .into_iter()
        .filter(|r| r.state == RecordState::Accepted)
        .collect();

    let conflicts = detect_conflicts(&accepted.iter().map(|r| (*r).clone()).collect::<Vec<_>>());

    let breadth_key = |r: &&KnowledgeRecord| (r.scope.breadth(), r.subject.clone(), r.id);
    let recency_key = |r: &&KnowledgeRecord| (std::cmp::Reverse(r.updated_at), r.id);

    let mut pinned: Vec<&KnowledgeRecord> = accepted.iter().copied().filter(|r| r.pinned).collect();
    pinned.sort_by_key(breadth_key);
    let mut rules: Vec<&KnowledgeRecord> = accepted
        .iter()
        .copied()
        .filter(|r| !r.pinned && r.is_rule())
        .collect();
    rules.sort_by_key(breadth_key);
    let mut decisions: Vec<&KnowledgeRecord> = accepted
        .iter()
        .copied()
        .filter(|r| !r.pinned && !r.is_rule() && r.kind == RecordKind::Human)
        .collect();
    decisions.sort_by_key(recency_key);
    let mut knowledge: Vec<&KnowledgeRecord> = accepted
        .iter()
        .copied()
        .filter(|r| !r.pinned && !r.is_rule() && r.kind != RecordKind::Human)
        .collect();
    knowledge.sort_by_key(recency_key);

    let mut tasks: Vec<&Task> = inputs
        .tasks
        .iter()
        .filter(|t| !t.status.is_terminal())
        .collect();
    tasks.sort_by_key(|t| (std::cmp::Reverse(t.updated_at), t.id));
    let mut maps: Vec<&ProjectMap> = inputs.project_maps.iter().collect();
    maps.sort_by(|a, b| a.project.cmp(&b.project));

    let mut candidates: Vec<Candidate> = Vec::new();
    candidates.extend(
        pinned
            .into_iter()
            .map(|r| record_candidate(Tier::Pinned, r)),
    );
    candidates.extend(rules.into_iter().map(|r| record_candidate(Tier::Rule, r)));
    candidates.extend(
        decisions
            .into_iter()
            .map(|r| record_candidate(Tier::Decision, r)),
    );
    candidates.extend(tasks.into_iter().map(task_candidate));
    candidates.extend(maps.into_iter().map(map_candidate));
    candidates.extend(
        knowledge
            .into_iter()
            .map(|r| record_candidate(Tier::Knowledge, r)),
    );

    let mut remaining = budget_tokens;
    let mut items = Vec::new();
    let mut omitted = Vec::new();
    for c in candidates {
        let tokens = estimate_tokens(&c.text);
        if tokens <= remaining {
            remaining -= tokens;
            items.push(BootstrapItem {
                tier: c.tier,
                title: c.title,
                text: c.text,
                source: c.source,
                tokens,
                truncated: false,
            });
            continue;
        }
        let cut = if remaining >= MIN_PARTIAL_TOKENS {
            truncate_to(&c.text, remaining)
        } else {
            None
        };
        match cut {
            Some(text) => {
                let used = estimate_tokens(&text);
                remaining = remaining.saturating_sub(used);
                items.push(BootstrapItem {
                    tier: c.tier,
                    title: c.title,
                    text,
                    source: c.source,
                    tokens: used,
                    truncated: true,
                });
            }
            None => omitted.push(OmittedItem {
                tier: c.tier,
                title: c.title,
                source: c.source,
                tokens,
            }),
        }
    }
    let used_tokens = items.iter().map(|i| i.tokens).sum();
    BootstrapPack {
        budget_tokens,
        used_tokens,
        items,
        omitted,
        stale_excluded,
        conflicts,
    }
}

impl BootstrapPack {
    /// Renders the pack as Markdown for an agent: contradictions first, then
    /// the items, then a statement of what was left out.
    pub fn render_markdown(&self) -> String {
        let mut out = String::from("# Knowell session pack\n");
        if !self.conflicts.is_empty() {
            out.push_str("\n## Conflicts (unresolved, do not guess)\n");
            for c in &self.conflicts {
                out.push_str(&format!(
                    "- `{}`: records {} ({}) and {} ({}) disagree\n",
                    c.subject,
                    c.first,
                    c.first_scope.key(),
                    c.second,
                    c.second_scope.key()
                ));
            }
        }
        for item in &self.items {
            out.push('\n');
            out.push_str(&item.text);
            out.push('\n');
        }
        if !self.omitted.is_empty() || self.stale_excluded > 0 {
            out.push_str("\n## Left out\n");
            if !self.omitted.is_empty() {
                out.push_str(&format!(
                    "{} item(s) did not fit the budget of {} tokens:\n",
                    self.omitted.len(),
                    self.budget_tokens
                ));
                for o in &self.omitted {
                    out.push_str(&format!(
                        "- {:?}: {} (~{} tokens)\n",
                        o.tier, o.title, o.tokens
                    ));
                }
            }
            if self.stale_excluded > 0 {
                out.push_str(&format!(
                    "{} stale record(s) are not shown because they are not current.\n",
                    self.stale_excluded
                ));
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ids::Subject;
    use crate::model::Actor;
    use crate::policy::{AcceptancePolicy, Rights};
    use crate::testutil::*;
    use uuid::Uuid;

    fn rec(
        n: u128,
        kind: RecordKind,
        subject: &str,
        tags: &[&str],
        pinned: bool,
        at: i64,
    ) -> KnowledgeRecord {
        let mut new = new_record(kind, n);
        new.subject = Subject::new(subject).unwrap();
        new.tags = tags.iter().map(|t| t.to_string()).collect();
        new.pinned = pinned;
        new.body = format!("body of {subject} {n}");
        let author = if kind == RecordKind::Observed {
            Actor::System
        } else {
            human("a")
        };
        let mut r = KnowledgeRecord::write(
            new,
            author,
            Rights::REVIEWER,
            &AcceptancePolicy::default(),
            ts(at),
        )
        .unwrap()
        .record;
        if r.state == RecordState::Proposed {
            r.accept(
                &human("rev"),
                Rights::REVIEWER,
                &AcceptancePolicy::default(),
                "ok",
                ts(at),
            )
            .unwrap();
        }
        r
    }

    fn task_with(n: u128, title: &str, at: i64) -> Task {
        Task::new(
            TaskId::from_uuid(Uuid::from_u128(n)),
            title,
            "goal text",
            ts(at),
        )
        .unwrap()
    }

    fn sample() -> BootstrapInputs {
        BootstrapInputs {
            records: vec![
                rec(1, RecordKind::Human, "dec.old", &[], false, 10),
                rec(2, RecordKind::Human, "dec.new", &[], false, 20),
                rec(3, RecordKind::Human, "rules.a", &["rule"], false, 5),
                rec(4, RecordKind::Human, "pin.me", &["rule"], true, 5),
                rec(5, RecordKind::Observed, "obs.fact", &[], false, 30),
            ],
            tasks: vec![task_with(1, "older task", 5), task_with(2, "newer task", 9)],
            project_maps: vec![
                ProjectMap {
                    project: name("web"),
                    summary: "web map".into(),
                    sources: vec![],
                },
                ProjectMap {
                    project: name("api"),
                    summary: "api map".into(),
                    sources: vec![evidence("src/main.rs", "h")],
                },
            ],
            scope_filter: None,
        }
    }

    fn titles(p: &BootstrapPack) -> Vec<String> {
        p.items.iter().map(|i| i.title.clone()).collect()
    }

    #[test]
    fn ranking_order() {
        let pack = bootstrap_pack(&sample(), 100_000);
        assert_eq!(
            titles(&pack),
            vec![
                "Record 4",
                "Record 3",
                "Record 2",
                "Record 1",
                "newer task",
                "older task",
                "Project map: api",
                "Project map: web",
                "Record 5",
            ]
        );
        let tiers: Vec<Tier> = pack.items.iter().map(|i| i.tier).collect();
        assert_eq!(
            tiers,
            vec![
                Tier::Pinned,
                Tier::Rule,
                Tier::Decision,
                Tier::Decision,
                Tier::OpenTask,
                Tier::OpenTask,
                Tier::ProjectMap,
                Tier::ProjectMap,
                Tier::Knowledge
            ]
        );
        assert!(pack.omitted.is_empty());
        assert_eq!(
            pack.used_tokens,
            pack.items.iter().map(|i| i.tokens).sum::<usize>()
        );
    }

    #[test]
    fn every_item_names_its_source() {
        let pack = bootstrap_pack(&sample(), 100_000);
        for item in &pack.items {
            assert!(
                item.source.record.is_some()
                    || item.source.task.is_some()
                    || item.source.project.is_some(),
                "{}",
                item.title
            );
        }
        let obs = pack.items.iter().find(|i| i.title == "Record 5").unwrap();
        assert_eq!(obs.source.record, Some(rid(5)));
        assert_eq!(obs.source.evidence.len(), 1);
        assert!(obs.text.contains("src/pay.rs#L10-L20"), "{}", obs.text);
        let map = pack
            .items
            .iter()
            .find(|i| i.title == "Project map: api")
            .unwrap();
        assert_eq!(map.source.evidence.len(), 1);
    }

    #[test]
    fn only_accepted_records_and_open_tasks() {
        let mut inputs = sample();
        let mut stale = rec(6, RecordKind::Human, "stale.one", &["rule"], true, 1);
        stale.mark_stale(&Actor::System, "x", ts(2)).unwrap();
        let proposed =
            KnowledgeRecord::propose(new_record(RecordKind::Human, 7), human("a"), ts(1)).unwrap();
        let mut rejected = rec(8, RecordKind::Human, "rej.one", &[], false, 1);
        rejected
            .reject(&human("r"), Rights::REVIEWER, "no", ts(2))
            .unwrap();
        inputs.records.extend([stale, proposed, rejected]);
        let mut done = task_with(3, "done task", 50);
        done.set_status(crate::task::TaskStatus::Done, ts(51))
            .unwrap();
        inputs.tasks.push(done);
        let pack = bootstrap_pack(&inputs, 100_000);
        let all = titles(&pack);
        assert!(
            !all.contains(&"Record 6".to_string()),
            "stale must not appear"
        );
        assert!(!all.contains(&"Record 7".to_string()));
        assert!(!all.contains(&"Record 8".to_string()));
        assert!(!all.contains(&"done task".to_string()));
        assert_eq!(pack.stale_excluded, 1);
        assert!(pack.render_markdown().contains("1 stale record(s)"));
    }

    #[test]
    fn scope_filter_limits_records() {
        let mut inputs = sample();
        inputs.records[0].scope = Scope::User(crate::ids::UserId::new("me").unwrap());
        inputs.scope_filter = Some(vec![project_scope("api")]);
        let pack = bootstrap_pack(&inputs, 100_000);
        assert!(!titles(&pack).contains(&"Record 1".to_string()));
        assert!(titles(&pack).contains(&"Record 2".to_string()));
    }

    #[test]
    fn budget_is_respected_and_omissions_listed() {
        let inputs = sample();
        let full = bootstrap_pack(&inputs, 100_000);
        let budget = full.items.iter().take(3).map(|i| i.tokens).sum::<usize>() + 5;
        let pack = bootstrap_pack(&inputs, budget);
        assert!(pack.used_tokens <= budget);
        assert_eq!(pack.items.len() + pack.omitted.len(), full.items.len());
        assert!(!pack.omitted.is_empty());
        assert_eq!(
            pack.items
                .iter()
                .take(3)
                .map(|i| i.title.clone())
                .collect::<Vec<_>>(),
            vec!["Record 4", "Record 3", "Record 2"]
        );
        let md = pack.render_markdown();
        assert!(md.contains("## Left out"));
        assert!(md.contains("did not fit the budget"));
    }

    #[test]
    fn zero_budget_omits_everything() {
        let pack = bootstrap_pack(&sample(), 0);
        assert!(pack.items.is_empty());
        assert_eq!(pack.omitted.len(), 9);
        assert_eq!(pack.used_tokens, 0);
    }

    #[test]
    fn oversized_item_is_truncated_not_dropped() {
        let mut inputs = BootstrapInputs::default();
        let mut big = rec(1, RecordKind::Human, "big.one", &["rule"], false, 1);
        big.body = "word ".repeat(2000);
        inputs.records.push(big);
        let pack = bootstrap_pack(&inputs, 100);
        assert_eq!(pack.items.len(), 1);
        assert!(pack.items[0].truncated);
        assert!(pack.items[0].tokens <= 100);
        assert!(pack.items[0].text.contains("truncated"));
        assert_eq!(pack.items[0].source.record, Some(rid(1)));
    }

    #[test]
    fn tiny_room_omits_instead_of_cutting() {
        let mut inputs = BootstrapInputs::default();
        let mut big = rec(1, RecordKind::Human, "big.one", &["rule"], false, 1);
        big.body = "word ".repeat(2000);
        inputs.records.push(big);
        let pack = bootstrap_pack(&inputs, 10);
        assert!(pack.items.is_empty());
        assert_eq!(pack.omitted.len(), 1);
    }

    #[test]
    fn truncation_is_char_boundary_safe() {
        let mut inputs = BootstrapInputs::default();
        let mut big = rec(1, RecordKind::Human, "big.one", &["rule"], false, 1);
        big.body = "ödeme ışık 日本語 ".repeat(500);
        inputs.records.push(big);
        let pack = bootstrap_pack(&inputs, 60);
        assert!(pack.items[0].tokens <= 60);
    }

    #[test]
    fn deterministic_regardless_of_input_order() {
        let a = sample();
        let mut b = sample();
        b.records.reverse();
        b.tasks.reverse();
        b.project_maps.reverse();
        for budget in [0, 50, 150, 400, 100_000] {
            assert_eq!(
                bootstrap_pack(&a, budget),
                bootstrap_pack(&b, budget),
                "budget {budget}"
            );
            assert_eq!(bootstrap_pack(&a, budget), bootstrap_pack(&a, budget));
        }
    }

    #[test]
    fn ties_break_by_id() {
        let mut inputs = BootstrapInputs::default();
        inputs.records.push(rec(
            2,
            RecordKind::Human,
            "same.subject",
            &["rule"],
            false,
            7,
        ));
        inputs.records.push(rec(
            1,
            RecordKind::Human,
            "same.subject",
            &["rule"],
            false,
            7,
        ));
        // Same subject, same body would not conflict; make bodies equal to avoid noise.
        let ids: Vec<_> = bootstrap_pack(&inputs, 10_000)
            .items
            .iter()
            .map(|i| i.source.record)
            .collect();
        assert_eq!(ids, vec![Some(rid(1)), Some(rid(2))]);
    }

    #[test]
    fn broader_scope_ranks_first_among_rules() {
        let mut inputs = BootstrapInputs::default();
        let mut project_rule = rec(1, RecordKind::Human, "a.rule", &["rule"], false, 1);
        project_rule.scope = project_scope("api");
        let mut org_rule = rec(2, RecordKind::Human, "z.rule", &["rule"], false, 1);
        org_rule.scope = Scope::Organization;
        inputs.records = vec![project_rule, org_rule];
        let ids: Vec<_> = bootstrap_pack(&inputs, 10_000)
            .items
            .iter()
            .map(|i| i.source.record)
            .collect();
        assert_eq!(ids, vec![Some(rid(2)), Some(rid(1))]);
    }

    #[test]
    fn conflicts_are_always_reported() {
        let mut inputs = BootstrapInputs::default();
        inputs
            .records
            .push(rec(1, RecordKind::Human, "pay.retry", &["rule"], false, 1));
        inputs
            .records
            .push(rec(2, RecordKind::Human, "pay.retry", &["rule"], false, 1));
        let pack = bootstrap_pack(&inputs, 0);
        assert_eq!(pack.conflicts.len(), 1);
        assert!(pack.render_markdown().contains("disagree"));
    }

    #[test]
    fn project_map_summary_is_redacted() {
        let mut inputs = BootstrapInputs::default();
        let token = fake_token();
        inputs.project_maps.push(ProjectMap {
            project: name("api"),
            summary: format!("run with {token}"),
            sources: vec![],
        });
        let pack = bootstrap_pack(&inputs, 1000);
        assert!(!pack.items[0].text.contains(&token));
        assert!(pack.items[0].text.contains("[REDACTED:"));
    }

    #[test]
    fn token_estimate_rounds_up() {
        assert_eq!(estimate_tokens(""), 0);
        assert_eq!(estimate_tokens("abcd"), 1);
        assert_eq!(estimate_tokens("abcde"), 2);
        assert_eq!(
            estimate_tokens("日本語日"),
            1,
            "counts characters, not bytes"
        );
    }
}
