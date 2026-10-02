//! Deterministic synthetic multi-repo workspace.
//!
//! The fixture is the online shop of the fictional company *Acme Goods*:
//! ten projects in seven languages (web storefront, mobile app, billing API,
//! orders service, payments ledger, notification worker, contracts, SQL
//! migrations, infrastructure, engineering handbook) wired together by
//! REST endpoints, Kafka events, a gRPC service and a shared database.
//!
//! - **Core files** are hand-written and carry the semantics every query is
//!   judged against. They are the same for every seed and scale.
//! - **Noise files** are generated per project in the project's language and
//!   scale with [`Scale`]; they are never relevant to a query.
//! - **Planted issues** (see [`planted_issues`]) give future checks ground
//!   truth: a schema drift, an undocumented endpoint, a committed `.env`
//!   file holding a canary, and a hard-coded fake access key.
//!
//! [`generate`] is pure and in-memory; [`Fixture::write_to`] materialises the
//! workspace, optionally with one deterministic git commit per project.
//! Output is byte-identical on every platform: all text uses `\n`, the
//! random stream is a fixed SplitMix64, and every collection is sorted.

mod core_files;
mod noise;
mod vocab;
mod write;

use std::collections::BTreeSet;
use std::fmt;
use std::fmt::Write as _;
use std::path::Path;
use std::str::FromStr;

use knowell_core::{ContentHash, Name, RepoPath};
use serde::{Deserialize, Serialize};

use crate::corpus::{Corpus, CorpusDoc};
use crate::error::EvalError;
use crate::rng::SplitMix64;

pub use write::{FixtureManifest, ManifestFile, ManifestProject, WriteOptions};

/// Name of the fixture workspace (also the name of its query set).
pub const FIXTURE_NAME: &str = "acme-goods";

/// Fixture size. Totals include the scale-independent core files.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Scale {
    /// 250 files in total.
    Small,
    /// 2 500 files in total.
    Medium,
    /// 25 000 files in total.
    Large,
}

impl Scale {
    /// Total number of files (core + noise) generated at this scale.
    pub fn target_files(self) -> usize {
        match self {
            Scale::Small => 250,
            Scale::Medium => 2_500,
            Scale::Large => 25_000,
        }
    }

    /// Lowercase name, as used in reports.
    pub fn as_str(self) -> &'static str {
        match self {
            Scale::Small => "small",
            Scale::Medium => "medium",
            Scale::Large => "large",
        }
    }
}

impl fmt::Display for Scale {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Error returned when parsing an unknown [`Scale`] name.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("unknown scale `{0}`; expected small, medium or large")]
pub struct ScaleParseError(String);

impl FromStr for Scale {
    type Err = ScaleParseError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "small" => Ok(Scale::Small),
            "medium" => Ok(Scale::Medium),
            "large" => Ok(Scale::Large),
            other => Err(ScaleParseError(other.to_owned())),
        }
    }
}

/// What to generate: the seed drives noise and the planted secret values;
/// the scale drives the number of noise files.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct FixtureSpec {
    /// Seed of the deterministic random stream.
    pub seed: u64,
    /// Workspace size.
    pub scale: Scale,
}

/// Whether a file is hand-written (judged by queries) or generated noise.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum FileRole {
    /// Hand-written, scale-independent; may be listed as relevant.
    Core,
    /// Generated; never listed as relevant.
    Noise,
}

/// One file of a project.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FixtureFile {
    /// Path relative to the project root.
    pub path: RepoPath,
    /// UTF-8 text with `\n` line endings.
    pub content: String,
    /// Core or noise.
    pub role: FileRole,
}

/// One project (one directory, optionally one git repository).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FixtureProject {
    /// Project name, also its directory name.
    pub name: Name,
    /// Primary language (`typescript`, `go`, `python`, …).
    pub language: String,
    /// Files sorted by path.
    pub files: Vec<FixtureFile>,
}

/// Identity of a generated fixture, carried into reports so that two reports
/// are only compared when they measured the same workspace.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FixtureInfo {
    /// Fixture name ([`FIXTURE_NAME`]).
    pub name: String,
    /// Generation seed.
    pub seed: u64,
    /// Generation scale.
    pub scale: Scale,
    /// [`Fixture::tree_hash`].
    pub tree_hash: ContentHash,
}

/// A deliberately planted defect with its ground truth.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct PlantedIssue {
    /// Stable identifier.
    pub id: &'static str,
    /// Category (`schema_drift`, `contract_drift`, `secret_file`, `hardcoded_secret`).
    pub kind: &'static str,
    /// What is wrong and what a correct tool should report.
    pub description: &'static str,
    /// Affected documents (`<project>/<path>`).
    pub docs: &'static [&'static str],
}

const PLANTED_ISSUES: &[PlantedIssue] = &[
    PlantedIssue {
        id: "schema-drift-cancel-reason",
        kind: "schema_drift",
        description: "Migration 0009 adds cancel_reason and cancel_feedback to subscriptions. \
                      billing-api's SubscriptionEntity maps both; ledger-service's SubscriptionRow \
                      read model of the same table was not updated.",
        docs: &[
            "db-migrations/migrations/0009_add_cancel_reason_to_subscriptions.sql",
            "billing-api/src/subscriptions/subscription.entity.ts",
            "ledger-service/ledger/db/models.py",
        ],
    },
    PlantedIssue {
        id: "contract-drift-resume-endpoint",
        kind: "contract_drift",
        description: "billing-api serves POST /v1/subscriptions/:id/resume and storefront-web \
                      calls it, but contracts/openapi/billing-api.yaml does not document it.",
        docs: &[
            "billing-api/src/subscriptions/subscriptions.controller.ts",
            "storefront-web/src/api/subscriptions.ts",
            "contracts/openapi/billing-api.yaml",
        ],
    },
    PlantedIssue {
        id: "committed-env-file",
        kind: "secret_file",
        description: "infra/.env is tracked and holds a canary SMTP password. It must be excluded \
                      by path before its content is read; the canary must never be indexed, \
                      logged or returned.",
        docs: &["infra/.env"],
    },
    PlantedIssue {
        id: "hardcoded-access-key",
        kind: "hardcoded_secret",
        description: "The order export job hard-codes a fake AWS-style access key id. The file \
                      may be indexed, but the key must be redacted first.",
        docs: &["orders-service/internal/export/s3_export.go"],
    },
];

/// The planted defects of the fixture (identical for every seed and scale).
pub fn planted_issues() -> &'static [PlantedIssue] {
    PLANTED_ISSUES
}

/// A generated workspace (in memory).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Fixture {
    spec: FixtureSpec,
    projects: Vec<FixtureProject>,
    canaries: Vec<String>,
}

/// Generates the fixture for `spec`. Pure: the same spec always yields the
/// same bytes on every platform. Small takes a few milliseconds.
pub fn generate(spec: &FixtureSpec) -> Fixture {
    let secrets = PlantedSecrets::derive(spec.seed);
    let noise_counts = distribute(
        spec.scale
            .target_files()
            .saturating_sub(core_files::core_file_count()),
        core_files::PROJECTS.iter().map(|p| p.noise_weight),
    );

    let mut projects = Vec::with_capacity(core_files::PROJECTS.len());
    for (project, noise_count) in core_files::PROJECTS.iter().zip(noise_counts) {
        let Ok(name) = Name::new(project.name) else {
            // Unreachable: project names are constants validated by tests.
            continue;
        };
        let mut used: BTreeSet<String> =
            project.files.iter().map(|(p, _)| (*p).to_owned()).collect();
        let mut files: Vec<FixtureFile> = project
            .files
            .iter()
            .filter_map(|(path, content)| {
                let content = core_files::normalize(content)
                    .replace(core_files::CANARY_PLACEHOLDER, &secrets.canary)
                    .replace(core_files::AWS_KEY_PLACEHOLDER, &secrets.aws_key_id);
                file(path, content, FileRole::Core)
            })
            .collect();

        let mut rng = SplitMix64::derive(spec.seed, project.name);
        let noise = noise::generate(project.noise_style, &mut rng, noise_count, &mut used);
        files.extend(
            noise
                .into_iter()
                .filter_map(|(path, content)| file(&path, content, FileRole::Noise)),
        );
        files.sort_by(|a, b| a.path.cmp(&b.path));
        projects.push(FixtureProject {
            name,
            language: project.language.to_owned(),
            files,
        });
    }

    Fixture {
        spec: *spec,
        projects,
        canaries: vec![secrets.canary, secrets.aws_key_id],
    }
}

fn file(path: &str, content: String, role: FileRole) -> Option<FixtureFile> {
    // Paths are built from validated constants; tests assert none is dropped.
    RepoPath::new(path).ok().map(|path| FixtureFile {
        path,
        content,
        role,
    })
}

/// Splits `total` proportionally to `weights` (largest remainder method,
/// ties broken by position) so the parts always sum to `total`.
fn distribute(total: usize, weights: impl Iterator<Item = usize>) -> Vec<usize> {
    let weights: Vec<usize> = weights.collect();
    let sum: usize = weights.iter().sum();
    if sum == 0 {
        return vec![0; weights.len()];
    }
    let mut parts: Vec<usize> = weights.iter().map(|w| total * w / sum).collect();
    let mut remainders: Vec<(usize, usize)> = weights
        .iter()
        .enumerate()
        .map(|(i, w)| (total * w % sum, i))
        .collect();
    // Largest remainder first; earlier position wins ties.
    remainders.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.cmp(&b.1)));
    let assigned: usize = parts.iter().sum();
    for (_, index) in remainders.into_iter().take(total.saturating_sub(assigned)) {
        if let Some(part) = parts.get_mut(index) {
            *part += 1;
        }
    }
    parts
}

/// The two seed-derived secret values planted in the fixture.
struct PlantedSecrets {
    canary: String,
    aws_key_id: String,
}

impl PlantedSecrets {
    fn derive(seed: u64) -> Self {
        const UPPER: &[u8; 26] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZ";
        let mut rng = SplitMix64::derive(seed, "knowell-eval/planted-secrets");
        let canary = format!("KNOWELL_CANARY_{:016x}", rng.next_u64());
        let mut aws_key_id = String::from("AKIA");
        for _ in 0..16 {
            if let Some(byte) = UPPER.get(rng.below(UPPER.len())) {
                aws_key_id.push(char::from(*byte));
            }
        }
        Self { canary, aws_key_id }
    }
}

impl Fixture {
    /// Fixture name ([`FIXTURE_NAME`]).
    pub fn name(&self) -> &'static str {
        FIXTURE_NAME
    }

    /// The spec this fixture was generated from.
    pub fn spec(&self) -> FixtureSpec {
        self.spec
    }

    /// Projects sorted by name.
    pub fn projects(&self) -> &[FixtureProject] {
        &self.projects
    }

    /// The planted secret values (the `.env` canary and the hard-coded
    /// access key id). Tests assert that none of them ever appears in an
    /// index, a log, an error or a report.
    pub fn canaries(&self) -> Vec<String> {
        self.canaries.clone()
    }

    /// Total number of files over all projects.
    pub fn file_count(&self) -> usize {
        self.projects.iter().map(|p| p.files.len()).sum()
    }

    /// Looks up a file by doc id (`<project>/<path>`).
    pub fn file(&self, doc_id: &str) -> Option<&FixtureFile> {
        let (project, path) = doc_id.split_once('/')?;
        let project = self.projects.iter().find(|p| p.name.as_str() == project)?;
        project
            .files
            .binary_search_by(|f| f.path.as_str().cmp(path))
            .ok()
            .and_then(|index| project.files.get(index))
    }

    /// BLAKE3 over every `(project, path, content)` triple in sorted order,
    /// prefixed with a format tag. Identical fixtures have identical hashes
    /// on every platform.
    pub fn tree_hash(&self) -> ContentHash {
        let tag: &[u8] = b"knowell-eval/fixture-tree/v1";
        let parts = std::iter::once(tag).chain(self.projects.iter().flat_map(|project| {
            project.files.iter().flat_map(move |f| {
                [
                    project.name.as_str().as_bytes(),
                    f.path.as_str().as_bytes(),
                    f.content.as_bytes(),
                ]
            })
        }));
        ContentHash::of_parts(parts)
    }

    /// Identity for reports.
    pub fn info(&self) -> FixtureInfo {
        FixtureInfo {
            name: FIXTURE_NAME.to_owned(),
            seed: self.spec.seed,
            scale: self.spec.scale,
            tree_hash: self.tree_hash(),
        }
    }

    /// The workspace configuration (`knowell.toml`) listing every project.
    pub fn workspace_toml(&self) -> String {
        let mut out = format!(
            "version = 1\n\n[workspace]\nname = \"{FIXTURE_NAME}\"\ntrack = \"branch:main\"\ndata_policy = \"local-only\"\n"
        );
        for project in &self.projects {
            // Writing to a String cannot fail.
            let _ = write!(
                out,
                "\n[[project]]\nname = \"{0}\"\npath = \"{0}\"\n",
                project.name
            );
        }
        out
    }

    /// Writes the workspace below `dir`: one directory per project plus a
    /// `knowell.toml` listing them. `dir` must not exist or be empty.
    ///
    /// With `options.git`, each project becomes its own git repository with
    /// one deterministic commit on `main` (identical commit ids on every
    /// machine); a missing `git` binary is reported as
    /// [`EvalError::GitNotFound`] before anything is written.
    pub fn write_to(
        &self,
        dir: &Path,
        options: &WriteOptions,
    ) -> Result<FixtureManifest, EvalError> {
        write::write_fixture(self, dir, options)
    }

    /// Every file as an in-memory corpus document with id `<project>/<path>`.
    ///
    /// This deliberately applies **no** secret exclusion or redaction: it is
    /// the raw workspace, for unit tests of retrievers and metrics. Measured
    /// runs build their corpus through the real file walker, which excludes
    /// `infra/.env` by path and redacts the hard-coded key; only that path is
    /// representative of what Knowell indexes.
    pub fn to_corpus(&self) -> Corpus {
        let docs = self.projects.iter().flat_map(|project| {
            project.files.iter().map(move |f| CorpusDoc {
                id: format!("{}/{}", project.name, f.path),
                text: f.content.clone(),
            })
        });
        Corpus::from_unique_sorted(docs.collect()).with_fixture(self.info())
    }
}

/// Doc ids of every core file, without generating noise.
pub(crate) fn core_doc_ids() -> Vec<String> {
    core_files::core_doc_ids()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn small(seed: u64) -> Fixture {
        generate(&FixtureSpec {
            seed,
            scale: Scale::Small,
        })
    }

    #[test]
    fn golden_tree_hash_seed_42_small() {
        // Pinned: any change to core files, noise templates, vocabulary or the
        // random stream changes this value. Update it deliberately (and say
        // so in the change description) because it invalidates baselines.
        assert_eq!(
            small(42).tree_hash().to_string(),
            "d6ea3857f12680c854a946e62cff25db18eb629645e1e485bc36eddfe989b2fc"
        );
    }

    #[test]
    fn generation_is_deterministic_and_seed_sensitive() {
        assert_eq!(small(7), small(7));
        assert_eq!(small(7).tree_hash(), small(7).tree_hash());
        assert_ne!(small(7).tree_hash(), small(8).tree_hash());
    }

    #[test]
    fn totals_match_scale_targets() {
        assert_eq!(small(1).file_count(), Scale::Small.target_files());
        let medium = generate(&FixtureSpec {
            seed: 1,
            scale: Scale::Medium,
        });
        assert_eq!(medium.file_count(), Scale::Medium.target_files());
        assert_eq!(medium.projects().len(), core_files::PROJECTS.len());
    }

    #[test]
    fn distribute_sums_to_total() {
        assert_eq!(distribute(10, [1, 1, 1].into_iter()), vec![4, 3, 3]);
        assert_eq!(distribute(0, [5, 5].into_iter()), vec![0, 0]);
        assert_eq!(distribute(7, [0, 0].into_iter()), vec![0, 0]);
        for total in [0, 1, 99, 136, 2_386, 24_886] {
            let parts = distribute(total, core_files::PROJECTS.iter().map(|p| p.noise_weight));
            assert_eq!(parts.iter().sum::<usize>(), total);
        }
    }

    #[test]
    fn core_files_are_identical_across_scales_and_seeds() {
        let core_of = |fixture: &Fixture| -> Vec<(String, String)> {
            fixture
                .projects()
                .iter()
                .flat_map(|p| {
                    p.files
                        .iter()
                        .filter(|f| f.role == FileRole::Core)
                        .filter(|f| {
                            f.path.as_str() != ".env" && !f.path.as_str().ends_with("s3_export.go")
                        })
                        .map(move |f| (format!("{}/{}", p.name, f.path), f.content.clone()))
                })
                .collect()
        };
        let a = core_of(&small(1));
        let b = core_of(&generate(&FixtureSpec {
            seed: 99,
            scale: Scale::Medium,
        }));
        assert_eq!(a, b);
        let ids: Vec<String> = small(1)
            .projects()
            .iter()
            .flat_map(|p| {
                p.files
                    .iter()
                    .filter(|f| f.role == FileRole::Core)
                    .map(move |f| format!("{}/{}", p.name, f.path))
            })
            .collect();
        let mut sorted = ids.clone();
        sorted.sort();
        assert_eq!(sorted, core_doc_ids());
    }

    #[test]
    fn canaries_are_seed_derived_and_planted_once() {
        let fixture = small(42);
        let canaries = fixture.canaries();
        assert_eq!(canaries.len(), 2);
        let canary = &canaries[0];
        let key = &canaries[1];
        assert!(
            canary.starts_with("KNOWELL_CANARY_") && canary.len() == 31,
            "{canary}"
        );
        assert!(key.starts_with("AKIA") && key.len() == 20);
        assert!(key[4..].chars().all(|c| c.is_ascii_uppercase()));
        assert_ne!(small(43).canaries(), canaries);

        let holders = |value: &str| -> Vec<String> {
            fixture
                .projects()
                .iter()
                .flat_map(|p| {
                    p.files
                        .iter()
                        .filter(|f| f.content.contains(value))
                        .map(move |f| format!("{}/{}", p.name, f.path))
                })
                .collect()
        };
        assert_eq!(holders(canary), ["infra/.env"]);
        assert_eq!(
            holders(key),
            ["orders-service/internal/export/s3_export.go"]
        );
        assert!(!fixture.workspace_toml().contains(canary.as_str()));
    }

    #[test]
    fn all_text_is_lf_only_and_paths_are_unique() {
        let fixture = generate(&FixtureSpec {
            seed: 3,
            scale: Scale::Medium,
        });
        for project in fixture.projects() {
            let mut seen = BTreeSet::new();
            for f in &project.files {
                assert!(!f.content.contains('\r'), "{}/{}", project.name, f.path);
                assert!(
                    seen.insert(f.path.as_str()),
                    "duplicate {}/{}",
                    project.name,
                    f.path
                );
            }
        }
    }

    #[test]
    fn absent_behaviour_never_appears() {
        // Ground truth of the `absent` queries: none of this exists anywhere.
        const MARKERS: &[&str] = &[
            "pdf",
            "invoice",
            "sms",
            "two-factor",
            "2fa",
            "totp",
            "loyalty",
            "sadakat",
            "cryptocurrency",
            "bitcoin",
            "kripto",
            "gift card",
            "gift_card",
            "giftcard",
            "hediye kart",
        ];
        let fixture = generate(&FixtureSpec {
            seed: 5,
            scale: Scale::Medium,
        });
        let canaries = fixture.canaries();
        for project in fixture.projects() {
            for f in &project.files {
                if canaries.iter().any(|c| f.content.contains(c.as_str())) {
                    continue; // random secret characters may spell anything
                }
                let lower = f.content.to_lowercase();
                for marker in MARKERS {
                    assert!(
                        !lower.contains(marker),
                        "{}/{} mentions `{marker}`",
                        project.name,
                        f.path
                    );
                }
            }
        }
    }

    #[test]
    fn noise_never_defines_core_symbols() {
        const SYMBOLS: &[&str] = &[
            "cancelSubscription",
            "SubscriptionService",
            "IdempotencyStore",
            "Idempotency-Key",
            "capture_payment",
            "CancelPendingForSubscription",
            "GetPaymentStatus",
            "SmtpMailer",
            "JwtAuthGuard",
            "EventPublisher",
            "SubscriptionScreen",
            "subscription.cancelled",
            "payment.captured",
            "SMTP_HOST",
            "DATABASE_URL",
            "/v1/subscriptions",
        ];
        let fixture = generate(&FixtureSpec {
            seed: 11,
            scale: Scale::Medium,
        });
        for project in fixture.projects() {
            for f in project.files.iter().filter(|f| f.role == FileRole::Noise) {
                for symbol in SYMBOLS {
                    assert!(
                        !f.content.contains(symbol),
                        "{}/{} contains `{symbol}`",
                        project.name,
                        f.path
                    );
                }
            }
        }
    }

    #[test]
    fn workspace_toml_lists_every_project() {
        let toml_text = small(1).workspace_toml();
        let value: toml::Value = toml::from_str(&toml_text).unwrap();
        assert_eq!(value["version"].as_integer(), Some(1));
        assert_eq!(value["workspace"]["name"].as_str(), Some(FIXTURE_NAME));
        assert_eq!(value["workspace"]["track"].as_str(), Some("branch:main"));
        assert_eq!(
            value["workspace"]["data_policy"].as_str(),
            Some("local-only")
        );
        let projects = value["project"].as_array().unwrap();
        assert_eq!(projects.len(), 10);
        assert_eq!(projects[0]["name"].as_str(), Some("billing-api"));
        assert_eq!(projects[0]["path"].as_str(), Some("billing-api"));
    }

    #[test]
    fn file_lookup_and_corpus() {
        let fixture = small(1);
        let f = fixture
            .file("ledger-service/ledger/payments/idempotency.py")
            .unwrap();
        assert_eq!(f.role, FileRole::Core);
        assert!(fixture.file("ledger-service/nope.py").is_none());
        assert!(fixture.file("nope").is_none());
        let corpus = fixture.to_corpus();
        assert_eq!(corpus.len(), fixture.file_count());
        assert_eq!(corpus.fixture(), Some(&fixture.info()));
        assert!(
            corpus.contains("infra/.env"),
            "to_corpus applies no exclusions by design"
        );
    }

    #[test]
    fn scale_parsing() {
        assert_eq!("medium".parse::<Scale>(), Ok(Scale::Medium));
        assert!("huge".parse::<Scale>().is_err());
        assert_eq!(Scale::Large.to_string(), "large");
    }

    #[test]
    fn planted_issue_docs_exist() {
        let fixture = small(1);
        for issue in planted_issues() {
            for doc in issue.docs {
                assert!(fixture.file(doc).is_some(), "{} -> {doc}", issue.id);
            }
        }
    }
}
