//! CI templates: GitHub Actions, GitLab CI and Gitea Actions.
//!
//! Generated files hold no secrets. Where credentials are needed they are
//! obtained at run time (OIDC tokens issued to the job) or referenced by CI
//! secret *name*. Third-party actions are pinned by tag with a comment
//! recommending a commit SHA.

use std::path::PathBuf;

use crate::error::SetupError;

/// CI systems with templates.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CiProvider {
    /// GitHub Actions.
    GitHub,
    /// GitLab CI/CD.
    GitLab,
    /// Gitea Actions (GitHub-Actions compatible).
    Gitea,
}

impl CiProvider {
    /// Name used on the command line.
    pub fn as_str(self) -> &'static str {
        match self {
            CiProvider::GitHub => "github",
            CiProvider::GitLab => "gitlab",
            CiProvider::Gitea => "gitea",
        }
    }
}

impl std::str::FromStr for CiProvider {
    type Err = SetupError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.to_ascii_lowercase().as_str() {
            "github" => Ok(CiProvider::GitHub),
            "gitlab" => Ok(CiProvider::GitLab),
            "gitea" => Ok(CiProvider::Gitea),
            _ => Err(SetupError::InvalidInput(format!(
                "unknown CI provider `{s}`; expected github, gitlab or gitea"
            ))),
        }
    }
}

/// What the generated pipeline does.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CiMode {
    /// `know check` on pull requests; needs no hub, no credentials, and is
    /// safe on fork pull requests.
    Check,
    /// `check` plus cross-project impact analysis through the hub on pull
    /// requests from the same repository (never on forks). Needs `hub_url`.
    CheckAndImpact,
    /// `check` on pull requests plus an index update on every push to
    /// `tracked_branch`. Needs `hub_url` and `tracked_branch`.
    IndexUpdate,
}

/// Options of [`ci_init`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CiOptions {
    /// Pipeline flavour.
    pub mode: CiMode,
    /// `https://` URL of the team's Knowell hub (no credentials in the URL).
    pub hub_url: Option<String>,
    /// Branch whose pushes update the index (the project's tracked branch).
    pub tracked_branch: Option<String>,
}

impl CiOptions {
    /// Check-only options.
    pub fn check() -> Self {
        Self {
            mode: CiMode::Check,
            hub_url: None,
            tracked_branch: None,
        }
    }
}

/// A file to write, path relative to the repository root.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GeneratedFile {
    /// Repository-relative path, `/`-separated.
    pub path: PathBuf,
    /// File content.
    pub content: String,
    /// Instruction for the user after writing the file, when one is needed.
    pub note: Option<String>,
}

const GITHUB_ACTION: &str = "knowell-dev/knowell-action@v1";
const SHA_HINT: &str = "# Security: tags can be moved. Prefer pinning to a full commit SHA, e.g.\n#   uses: knowell-dev/knowell-action@<40-character-sha>  # v1.x.y\n";

fn safe_url(url: &str) -> bool {
    url.strip_prefix("https://").is_some_and(|rest| {
        let host = rest.split('/').next().unwrap_or("");
        !host.is_empty()
            && !host.contains('@')
            && rest
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || "._~:/?@+,;=%-".contains(c))
    })
}

fn safe_branch(branch: &str) -> bool {
    !branch.is_empty()
        && !branch.starts_with(['/', '-', '.'])
        && !branch.ends_with(['/', '.'])
        && !branch.contains("..")
        && !branch.contains("//")
        && branch
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "._/-".contains(c))
}

struct Validated<'a> {
    hub: Option<&'a str>,
    branch: Option<&'a str>,
}

fn validate(opts: &CiOptions) -> Result<Validated<'_>, SetupError> {
    let invalid = |m: &str| Err(SetupError::InvalidInput(m.to_owned()));
    let hub = match opts.hub_url.as_deref() {
        Some(u) if !safe_url(u) => {
            return invalid(
                "hub_url must be an https:// URL without credentials, spaces or quotes",
            );
        }
        other => other,
    };
    let branch = match opts.tracked_branch.as_deref() {
        Some(b) if !safe_branch(b) => {
            return invalid("tracked_branch contains characters not allowed in a CI template");
        }
        other => other,
    };
    match opts.mode {
        CiMode::Check => {}
        CiMode::CheckAndImpact if hub.is_none() => {
            return invalid("mode check-and-impact needs hub_url");
        }
        CiMode::IndexUpdate if hub.is_none() || branch.is_none() => {
            return invalid("mode index-update needs hub_url and tracked_branch");
        }
        _ => {}
    }
    Ok(Validated { hub, branch })
}

/// Generates the CI files for `provider`.
///
/// # Errors
///
/// Returns [`SetupError::InvalidInput`] when the mode needs options that are
/// missing, or when `hub_url` / `tracked_branch` are not safe to embed.
pub fn ci_init(provider: CiProvider, opts: &CiOptions) -> Result<Vec<GeneratedFile>, SetupError> {
    let v = validate(opts)?;
    Ok(match provider {
        CiProvider::GitHub => vec![GeneratedFile {
            path: PathBuf::from(".github/workflows/knowell.yml"),
            content: actions_workflow(opts.mode, &v, true),
            note: None,
        }],
        CiProvider::Gitea => vec![GeneratedFile {
            path: PathBuf::from(".gitea/workflows/knowell.yml"),
            content: actions_workflow(opts.mode, &v, false),
            note: Some(
                "Gitea has no OIDC token for jobs: add the repository secret KNOWELL_HUB_TOKEN (issued by your hub) before enabling impact or index jobs"
                    .to_owned(),
            ),
        }],
        CiProvider::GitLab => vec![GeneratedFile {
            path: PathBuf::from(".gitlab/knowell.gitlab-ci.yml"),
            content: gitlab_pipeline(opts.mode, &v),
            note: Some(
                "add `include: [{ local: .gitlab/knowell.gitlab-ci.yml }]` to .gitlab-ci.yml"
                    .to_owned(),
            ),
        }],
    })
}

/// GitHub Actions and Gitea Actions share the workflow syntax; `github` adds
/// OIDC and code-scanning permissions.
fn actions_workflow(mode: CiMode, v: &Validated<'_>, github: bool) -> String {
    let mut out = String::new();
    let system = if github {
        "GitHub Actions"
    } else {
        "Gitea Actions"
    };
    out.push_str(&format!(
        "# Knowell checks for {system}. Generated by `know ci init {}`; safe to edit.\n",
        if github { "github" } else { "gitea" }
    ));
    out.push_str(SHA_HINT);
    if !github {
        out.push_str(
            "# Gitea resolves `uses:` through its configured default actions URL; mirror\n# the action there or use the full URL form if github.com is not the default.\n",
        );
    }
    out.push_str("name: Knowell\n\non:\n  pull_request:\n");
    if let (CiMode::IndexUpdate, Some(branch)) = (mode, v.branch) {
        out.push_str(&format!("  push:\n    branches: [\"{branch}\"]\n"));
    }
    out.push_str("\npermissions:\n  contents: read\n\njobs:\n");

    // Check: no secrets, runs on forks too.
    out.push_str("  check:\n    runs-on: ubuntu-latest\n");
    if github {
        out.push_str("    permissions:\n      contents: read\n      security-events: write  # SARIF upload to code scanning\n");
    }
    out.push_str("    if: github.event_name == 'pull_request'\n    steps:\n");
    out.push_str("      - uses: actions/checkout@v4\n        with:\n          fetch-depth: 0\n");
    out.push_str(&format!(
        "      - uses: {GITHUB_ACTION}\n        with:\n          command: check\n"
    ));
    if github {
        out.push_str("          sarif: true\n");
    }

    let Some(hub) = v.hub else { return out };
    // Never on fork pull requests: those jobs must not receive credentials.
    let same_repo = "github.event.pull_request.head.repo.full_name == github.repository";
    let (perms, creds) = if github {
        (
            "    permissions:\n      contents: read\n      id-token: write  # GitHub OIDC token for the hub; no stored secret\n      pull-requests: write\n",
            String::new(),
        )
    } else {
        (
            "    permissions:\n      contents: read\n      pull-requests: write\n",
            "        env:\n          KNOWELL_OIDC_TOKEN: ${{ secrets.KNOWELL_HUB_TOKEN }}\n"
                .to_owned(),
        )
    };
    if matches!(mode, CiMode::CheckAndImpact) {
        out.push_str(&format!(
            "\n  impact:\n    runs-on: ubuntu-latest\n    if: github.event_name == 'pull_request' && {same_repo}\n{perms}    steps:\n      - uses: actions/checkout@v4\n        with:\n          fetch-depth: 0\n      - uses: {GITHUB_ACTION}\n        with:\n          command: impact\n          hub-url: {hub}\n{creds}"
        ));
    }
    if let (CiMode::IndexUpdate, Some(branch)) = (mode, v.branch) {
        let perms = perms.replace("      pull-requests: write\n", "");
        out.push_str(&format!(
            "\n  index:\n    runs-on: ubuntu-latest\n    if: github.event_name == 'push' && github.ref == 'refs/heads/{branch}'\n{perms}    steps:\n      - uses: actions/checkout@v4\n        with:\n          fetch-depth: 0\n      - uses: {GITHUB_ACTION}\n        with:\n          command: index\n          hub-url: {hub}\n{creds}"
        ));
    }
    out
}

fn gitlab_pipeline(mode: CiMode, v: &Validated<'_>) -> String {
    let mut out = String::new();
    out.push_str(
        "# Knowell checks for GitLab CI. Generated by `know ci init gitlab`; safe to edit.\n",
    );
    out.push_str("# Security: pin the image by digest (image: ghcr.io/knowell-dev/knowell@sha256:<digest>)\n# instead of a moving tag.\n");
    out.push_str("\nvariables:\n  KNOWELL_IMAGE: ghcr.io/knowell-dev/knowell:1\n");
    out.push_str("\n.knowell-base:\n  image: $KNOWELL_IMAGE\n  stage: test\n  variables:\n    GIT_DEPTH: \"0\"\n");

    out.push_str("\nknowell-check:\n  extends: .knowell-base\n  rules:\n    - if: $CI_PIPELINE_SOURCE == \"merge_request_event\"\n");
    out.push_str("  script:\n    - know check --format sarif --output knowell.sarif --fail-on error\n  artifacts:\n    when: always\n    paths:\n      - knowell.sarif\n    reports:\n      sast: knowell.sarif\n");

    let Some(hub) = v.hub else { return out };
    // GitLab issues the OIDC token to the job; the hub verifies it.
    let id_tokens = format!("  id_tokens:\n    KNOWELL_OIDC_TOKEN:\n      aud: {hub}\n");
    if matches!(mode, CiMode::CheckAndImpact) {
        out.push_str(&format!(
            "\nknowell-impact:\n  extends: .knowell-base\n  rules:\n    # Not for merge requests from forks: those jobs must not get credentials.\n    - if: $CI_PIPELINE_SOURCE == \"merge_request_event\" && $CI_MERGE_REQUEST_SOURCE_PROJECT_ID == $CI_PROJECT_ID\n{id_tokens}  script:\n    - know impact --hub {hub} --oidc-audience {hub} --format markdown --output knowell-impact.md\n  artifacts:\n    when: always\n    paths:\n      - knowell-impact.md\n"
        ));
    }
    if let (CiMode::IndexUpdate, Some(branch)) = (mode, v.branch) {
        out.push_str(&format!(
            "\nknowell-index:\n  extends: .knowell-base\n  rules:\n    - if: $CI_COMMIT_BRANCH == \"{branch}\"\n{id_tokens}  script:\n    - know index --hub {hub} --oidc-audience {hub} --commit $CI_COMMIT_SHA --ref refs/heads/{branch}\n"
        ));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn opts(mode: CiMode) -> CiOptions {
        CiOptions {
            mode,
            hub_url: Some("https://hub.example.com".into()),
            tracked_branch: Some("main".into()),
        }
    }

    fn one(provider: CiProvider, o: &CiOptions) -> GeneratedFile {
        let mut files = ci_init(provider, o).unwrap();
        assert_eq!(files.len(), 1);
        files.remove(0)
    }

    #[test]
    fn github_check_snapshot() {
        let f = one(CiProvider::GitHub, &CiOptions::check());
        assert_eq!(f.path, PathBuf::from(".github/workflows/knowell.yml"));
        let expected = "\
# Knowell checks for GitHub Actions. Generated by `know ci init github`; safe to edit.
# Security: tags can be moved. Prefer pinning to a full commit SHA, e.g.
#   uses: knowell-dev/knowell-action@<40-character-sha>  # v1.x.y
name: Knowell

on:
  pull_request:

permissions:
  contents: read

jobs:
  check:
    runs-on: ubuntu-latest
    permissions:
      contents: read
      security-events: write  # SARIF upload to code scanning
    if: github.event_name == 'pull_request'
    steps:
      - uses: actions/checkout@v4
        with:
          fetch-depth: 0
      - uses: knowell-dev/knowell-action@v1
        with:
          command: check
          sarif: true
";
        assert_eq!(f.content, expected);
    }

    #[test]
    fn github_impact_uses_oidc_and_skips_forks() {
        let f = one(CiProvider::GitHub, &opts(CiMode::CheckAndImpact));
        let c = &f.content;
        assert!(c.contains("  impact:\n"));
        assert!(c.contains("id-token: write"));
        assert!(c.contains("github.event.pull_request.head.repo.full_name == github.repository"));
        assert!(c.contains("command: impact\n          hub-url: https://hub.example.com\n"));
        assert!(!c.contains("  index:"));
        assert!(!c.contains("push:"));
    }

    #[test]
    fn github_index_runs_on_tracked_branch_push() {
        let f = one(CiProvider::GitHub, &opts(CiMode::IndexUpdate));
        let c = &f.content;
        assert!(c.contains("  push:\n    branches: [\"main\"]\n"));
        assert!(c.contains("if: github.event_name == 'push' && github.ref == 'refs/heads/main'"));
        assert!(c.contains("command: index"));
        assert!(!c.contains("  impact:"));
        assert!(c.contains("  check:\n"));
        // The index job must not get pull-request write access.
        let index_part = c.split("  index:\n").nth(1).unwrap();
        assert!(!index_part.contains("pull-requests"));
        assert!(index_part.contains("id-token: write"));
    }

    #[test]
    fn gitea_references_secret_names_only() {
        let f = one(CiProvider::Gitea, &opts(CiMode::CheckAndImpact));
        assert_eq!(f.path, PathBuf::from(".gitea/workflows/knowell.yml"));
        assert!(f.content.contains("${{ secrets.KNOWELL_HUB_TOKEN }}"));
        assert!(!f.content.contains("id-token"));
        assert!(!f.content.contains("security-events"));
        assert!(f.note.as_deref().unwrap().contains("KNOWELL_HUB_TOKEN"));
    }

    #[test]
    fn gitlab_pipeline_shapes() {
        let check = one(CiProvider::GitLab, &CiOptions::check());
        assert_eq!(check.path, PathBuf::from(".gitlab/knowell.gitlab-ci.yml"));
        assert!(check.content.contains("know check --format sarif"));
        assert!(!check.content.contains("id_tokens"));
        assert!(check.note.as_deref().unwrap().contains("include"));

        let impact = one(CiProvider::GitLab, &opts(CiMode::CheckAndImpact));
        assert!(impact.content.contains("knowell-impact:"));
        assert!(impact.content.contains("aud: https://hub.example.com"));
        assert!(
            impact
                .content
                .contains("CI_MERGE_REQUEST_SOURCE_PROJECT_ID == $CI_PROJECT_ID")
        );

        let index = one(CiProvider::GitLab, &opts(CiMode::IndexUpdate));
        assert!(index.content.contains("$CI_COMMIT_BRANCH == \"main\""));
        assert!(
            index
                .content
                .contains("know index --hub https://hub.example.com")
        );
    }

    #[test]
    fn templates_are_yaml_shaped_and_secret_free() {
        for provider in [CiProvider::GitHub, CiProvider::GitLab, CiProvider::Gitea] {
            for mode in [CiMode::Check, CiMode::CheckAndImpact, CiMode::IndexUpdate] {
                let f = one(provider, &opts(mode));
                assert!(
                    !f.content.contains('\t'),
                    "tabs are invalid YAML indentation"
                );
                assert!(f.content.ends_with('\n'));
                for (i, line) in f.content.lines().enumerate() {
                    let indent = line.len() - line.trim_start().len();
                    assert_eq!(
                        indent % 2,
                        0,
                        "{provider:?} {mode:?} line {}: {line}",
                        i + 1
                    );
                }
                // Secrets may only be referenced by an upper-case name.
                for part in f.content.split("secrets.").skip(1) {
                    let name: String = part
                        .chars()
                        .take_while(|c| c.is_ascii_uppercase() || *c == '_')
                        .collect();
                    assert!(!name.is_empty());
                }
                for forbidden in ["ghp_", "glpat-", "Bearer ", "password", "AKIA"] {
                    assert!(!f.content.contains(forbidden), "{forbidden}");
                }
                assert!(f.content.contains("Security:"), "pinning advice present");
            }
        }
    }

    #[test]
    fn missing_or_unsafe_options_are_rejected() {
        let mut o = opts(CiMode::CheckAndImpact);
        o.hub_url = None;
        assert!(ci_init(CiProvider::GitHub, &o).is_err());
        let mut o = opts(CiMode::IndexUpdate);
        o.tracked_branch = None;
        assert!(ci_init(CiProvider::GitLab, &o).is_err());
        for bad in [
            "http://hub.example.com",
            "https://user:pw@hub.example.com",
            "https://tok@hub.example.com",
            "https://hub.example.com/a b",
            "https://hub.example.com\nrun: evil",
            "https://",
            "https://hub.example.com/\"x",
        ] {
            let mut o = opts(CiMode::CheckAndImpact);
            o.hub_url = Some(bad.into());
            assert!(ci_init(CiProvider::GitHub, &o).is_err(), "{bad:?}");
        }
        for bad in ["", "-x", "a b", "a\"b", "a$(x)", "../x", "x\ny"] {
            let mut o = opts(CiMode::IndexUpdate);
            o.tracked_branch = Some(bad.into());
            assert!(ci_init(CiProvider::GitHub, &o).is_err(), "{bad:?}");
        }
        let mut o = opts(CiMode::Check);
        o.tracked_branch = Some("release/2.x".into());
        assert!(ci_init(CiProvider::GitHub, &o).is_ok());
    }

    #[test]
    fn provider_names_parse() {
        assert_eq!("GitHub".parse::<CiProvider>().unwrap(), CiProvider::GitHub);
        assert_eq!(CiProvider::Gitea.as_str(), "gitea");
        assert!("svn".parse::<CiProvider>().is_err());
    }
}
