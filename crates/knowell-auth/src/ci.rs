//! CI identity: verified OIDC claims and the policy that maps them to the
//! actions a CI job may perform.
//!
//! JWT signature verification is deliberately not here. An [`OidcVerifier`]
//! implementation (added later, next to the HTTP client and JWKS cache) turns a
//! presented token into [`CiClaims`]; this module only decides what verified
//! claims are allowed to do. Claims must never be built from an unverified
//! token.

use std::collections::BTreeSet;

use serde::{Deserialize, Serialize};
use time::OffsetDateTime;

/// Verified claims of a CI job identity token (GitHub Actions naming).
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub struct CiClaims {
    /// Token issuer, e.g. `https://token.actions.githubusercontent.com`.
    pub issuer: String,
    /// `owner/name` of the repository the workflow runs in.
    pub repository: String,
    /// Full git ref, e.g. `refs/heads/main`.
    pub git_ref: String,
    /// Workflow name or path.
    pub workflow: String,
    /// Deployment environment, if the job uses one.
    pub environment: Option<String>,
    /// Triggering event, e.g. `push` or `pull_request`, when present.
    pub event_name: Option<String>,
}

/// Why verification of a CI token failed. Messages never contain the token.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum OidcError {
    /// The token is not a well-formed JWT.
    #[error("malformed identity token")]
    Malformed,
    /// The signature, issuer, audience or time window did not verify.
    #[error("identity token rejected")]
    Rejected,
    /// The verifier could not reach its key source.
    #[error("identity token keys unavailable")]
    Unavailable,
}

/// Verifies a CI identity token and returns its claims.
///
/// Implementations must check the signature against the issuer's keys, the
/// audience, and the `nbf`/`exp` window against `now`, and must fail closed.
pub trait OidcVerifier {
    /// Verifies `token` at time `now`.
    ///
    /// # Errors
    /// [`OidcError`] when the token cannot be proven valid.
    fn verify(&self, token: &str, now: OffsetDateTime) -> Result<CiClaims, OidcError>;
}

/// Something a CI job may be allowed to do.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CiAction {
    /// Ask the hub to update an index to the pushed commit.
    TriggerIndexUpdate,
    /// Upload a prebuilt, attested index bundle.
    UploadIndexBundle,
    /// Read index freshness and status.
    ReadIndexStatus,
}

/// Errors building a [`CiRule`] or [`CiPolicy`].
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum CiPolicyError {
    /// A field that must be non-empty is empty.
    #[error("{0} must not be empty")]
    Empty(&'static str),
    /// The ref pattern is neither an exact ref nor `refs/heads/<prefix>/*` /
    /// `refs/tags/<prefix>/*`.
    #[error("ref pattern must be an exact ref or end in `/*` under refs/heads/ or refs/tags/")]
    InvalidRefPattern,
}

/// One allow rule: claims matching the ref (and optionally workflow and
/// environment) are granted `actions`.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct CiRule {
    ref_pattern: String,
    workflow: Option<String>,
    environment: Option<String>,
    actions: BTreeSet<CiAction>,
}

impl CiRule {
    /// A rule for `ref_pattern`: an exact ref such as `refs/heads/main`, or a
    /// prefix wildcard such as `refs/heads/release/*` (never broader than a
    /// branch or tag namespace, so `refs/pull/*` cannot be matched by
    /// accident).
    ///
    /// # Errors
    /// [`CiPolicyError`] for an empty or too-broad pattern or empty action set.
    pub fn new(
        ref_pattern: impl Into<String>,
        actions: impl IntoIterator<Item = CiAction>,
    ) -> Result<Self, CiPolicyError> {
        let ref_pattern = ref_pattern.into();
        let actions: BTreeSet<CiAction> = actions.into_iter().collect();
        if actions.is_empty() {
            return Err(CiPolicyError::Empty("actions"));
        }
        if ref_pattern.is_empty() {
            return Err(CiPolicyError::Empty("ref pattern"));
        }
        if let Some(prefix) = ref_pattern.strip_suffix('*') {
            let namespaced = prefix.ends_with('/')
                && (prefix.starts_with("refs/heads/") || prefix.starts_with("refs/tags/"));
            if !namespaced || prefix.contains('*') {
                return Err(CiPolicyError::InvalidRefPattern);
            }
        } else if ref_pattern.contains('*') {
            return Err(CiPolicyError::InvalidRefPattern);
        }
        Ok(Self {
            ref_pattern,
            workflow: None,
            environment: None,
            actions,
        })
    }

    /// Requires the claim's workflow to equal `workflow`.
    ///
    /// # Errors
    /// [`CiPolicyError::Empty`] for empty text.
    pub fn workflow(mut self, workflow: impl Into<String>) -> Result<Self, CiPolicyError> {
        let workflow = workflow.into();
        if workflow.is_empty() {
            return Err(CiPolicyError::Empty("workflow"));
        }
        self.workflow = Some(workflow);
        Ok(self)
    }

    /// Requires the claim's environment to equal `environment`.
    ///
    /// # Errors
    /// [`CiPolicyError::Empty`] for empty text.
    pub fn environment(mut self, environment: impl Into<String>) -> Result<Self, CiPolicyError> {
        let environment = environment.into();
        if environment.is_empty() {
            return Err(CiPolicyError::Empty("environment"));
        }
        self.environment = Some(environment);
        Ok(self)
    }

    fn matches(&self, claims: &CiClaims) -> bool {
        let ref_ok = match self.ref_pattern.strip_suffix('*') {
            Some(prefix) => claims.git_ref.starts_with(prefix),
            None => claims.git_ref == self.ref_pattern,
        };
        ref_ok
            && self
                .workflow
                .as_deref()
                .is_none_or(|w| w == claims.workflow)
            && self
                .environment
                .as_deref()
                .is_none_or(|e| claims.environment.as_deref() == Some(e))
    }
}

/// Why a [`CiDecision`] is what it is. [`CiReason::code`] is stable.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum CiReason {
    /// At least one rule matched.
    Matched,
    /// A required claim is empty.
    IncompleteClaims,
    /// The issuer is not the configured one.
    IssuerMismatch,
    /// The repository is not the configured one (forks land here).
    RepositoryMismatch,
    /// The triggering event is never trusted (`pull_request_target`).
    ForbiddenEvent,
    /// The identity is right but no rule covers this ref/workflow/environment.
    NoRuleMatched,
}

impl CiReason {
    /// Stable snake_case code.
    pub fn code(&self) -> &'static str {
        match self {
            Self::Matched => "matched",
            Self::IncompleteClaims => "incomplete_claims",
            Self::IssuerMismatch => "issuer_mismatch",
            Self::RepositoryMismatch => "repository_mismatch",
            Self::ForbiddenEvent => "forbidden_event",
            Self::NoRuleMatched => "no_rule_matched",
        }
    }
}

/// What a set of claims may do.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct CiDecision {
    /// Allowed actions (empty when denied).
    pub allowed: BTreeSet<CiAction>,
    /// Why.
    pub reason: CiReason,
}

impl CiDecision {
    /// True when `action` is allowed.
    pub fn permits(&self, action: CiAction) -> bool {
        self.allowed.contains(&action)
    }

    fn deny(reason: CiReason) -> Self {
        Self {
            allowed: BTreeSet::new(),
            reason,
        }
    }
}

/// Maps verified CI claims to allowed actions for one repository.
///
/// Deny by default. The union of all matching rules applies. Additional
/// hard rules: the issuer must match exactly; the repository must match
/// (ASCII case-insensitively, as GitHub names are); `pull_request_target`
/// events get nothing because they run with base-repository identity on
/// untrusted code; and fork or pull-request refs (`refs/pull/...`) can only
/// match a rule that names them exactly, which [`CiRule::new`] makes
/// impossible to do by wildcard.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct CiPolicy {
    issuer: String,
    repository: String,
    rules: Vec<CiRule>,
}

impl CiPolicy {
    /// A policy for `repository` (`owner/name`) with tokens from `issuer`.
    ///
    /// # Errors
    /// [`CiPolicyError::Empty`] when either is empty.
    pub fn new(
        issuer: impl Into<String>,
        repository: impl Into<String>,
    ) -> Result<Self, CiPolicyError> {
        let issuer = issuer.into();
        let repository = repository.into();
        if issuer.is_empty() {
            return Err(CiPolicyError::Empty("issuer"));
        }
        if repository.is_empty() {
            return Err(CiPolicyError::Empty("repository"));
        }
        Ok(Self {
            issuer,
            repository,
            rules: Vec::new(),
        })
    }

    /// Adds a rule.
    pub fn with_rule(mut self, rule: CiRule) -> Self {
        self.rules.push(rule);
        self
    }

    /// Evaluates verified `claims`.
    pub fn evaluate(&self, claims: &CiClaims) -> CiDecision {
        if claims.issuer.is_empty() || claims.repository.is_empty() || claims.git_ref.is_empty() {
            return CiDecision::deny(CiReason::IncompleteClaims);
        }
        if claims.issuer != self.issuer {
            return CiDecision::deny(CiReason::IssuerMismatch);
        }
        if !claims.repository.eq_ignore_ascii_case(&self.repository) {
            return CiDecision::deny(CiReason::RepositoryMismatch);
        }
        if claims.event_name.as_deref() == Some("pull_request_target") {
            return CiDecision::deny(CiReason::ForbiddenEvent);
        }
        let allowed: BTreeSet<CiAction> = self
            .rules
            .iter()
            .filter(|r| r.matches(claims))
            .flat_map(|r| r.actions.iter().copied())
            .collect();
        if allowed.is_empty() {
            CiDecision::deny(CiReason::NoRuleMatched)
        } else {
            CiDecision {
                allowed,
                reason: CiReason::Matched,
            }
        }
    }

    /// Shorthand: may these claims perform `action`?
    pub fn permits(&self, claims: &CiClaims, action: CiAction) -> bool {
        self.evaluate(claims).permits(action)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const ISSUER: &str = "https://token.actions.githubusercontent.com";

    fn claims(repo: &str, git_ref: &str) -> CiClaims {
        CiClaims {
            issuer: ISSUER.to_owned(),
            repository: repo.to_owned(),
            git_ref: git_ref.to_owned(),
            workflow: "index.yml".to_owned(),
            environment: None,
            event_name: Some("push".to_owned()),
        }
    }

    fn policy() -> CiPolicy {
        CiPolicy::new(ISSUER, "acme/widgets")
            .unwrap()
            .with_rule(
                CiRule::new(
                    "refs/heads/main",
                    [CiAction::TriggerIndexUpdate, CiAction::ReadIndexStatus],
                )
                .unwrap(),
            )
            .with_rule(CiRule::new("refs/heads/*", [CiAction::ReadIndexStatus]).unwrap())
    }

    #[test]
    fn main_of_configured_repo_may_trigger() {
        let d = policy().evaluate(&claims("acme/widgets", "refs/heads/main"));
        assert_eq!(d.reason, CiReason::Matched);
        assert!(d.permits(CiAction::TriggerIndexUpdate));
        assert!(d.permits(CiAction::ReadIndexStatus));
        assert!(!d.permits(CiAction::UploadIndexBundle));
    }

    #[test]
    fn repository_match_ignores_case() {
        assert!(policy().permits(
            &claims("Acme/Widgets", "refs/heads/main"),
            CiAction::TriggerIndexUpdate
        ));
    }

    #[test]
    fn other_branches_cannot_trigger() {
        let p = policy();
        let d = p.evaluate(&claims("acme/widgets", "refs/heads/feature/x"));
        assert!(!d.permits(CiAction::TriggerIndexUpdate));
        assert!(d.permits(CiAction::ReadIndexStatus));
        assert!(!p.permits(
            &claims("acme/widgets", "refs/heads/main2"),
            CiAction::TriggerIndexUpdate
        ));
        assert!(!p.permits(
            &claims("acme/widgets", "refs/tags/main"),
            CiAction::TriggerIndexUpdate
        ));
    }

    #[test]
    fn forks_get_nothing() {
        let p = policy();
        for repo in [
            "mallory/widgets",
            "acme/widgets-fork",
            "acme/widgets/x",
            "acme",
        ] {
            let d = p.evaluate(&claims(repo, "refs/heads/main"));
            assert_eq!(d.reason, CiReason::RepositoryMismatch, "{repo}");
            assert!(d.allowed.is_empty());
        }
    }

    #[test]
    fn pull_request_refs_match_no_wildcard() {
        let p = policy();
        let d = p.evaluate(&claims("acme/widgets", "refs/pull/7/merge"));
        assert_eq!(d.reason, CiReason::NoRuleMatched);
        assert!(d.allowed.is_empty());
    }

    #[test]
    fn pull_request_target_is_never_trusted() {
        let mut c = claims("acme/widgets", "refs/heads/main");
        c.event_name = Some("pull_request_target".to_owned());
        let d = policy().evaluate(&c);
        assert_eq!(d.reason, CiReason::ForbiddenEvent);
        assert!(d.allowed.is_empty());
    }

    #[test]
    fn issuer_must_match_exactly() {
        let mut c = claims("acme/widgets", "refs/heads/main");
        for bad in [
            "https://token.actions.githubusercontent.com/",
            "https://evil.example",
            "HTTPS://TOKEN.ACTIONS.GITHUBUSERCONTENT.COM",
        ] {
            c.issuer = bad.to_owned();
            assert_eq!(policy().evaluate(&c).reason, CiReason::IssuerMismatch);
        }
    }

    #[test]
    fn incomplete_claims_denied() {
        for mutate in [
            |c: &mut CiClaims| c.issuer.clear(),
            |c: &mut CiClaims| c.repository.clear(),
            |c: &mut CiClaims| c.git_ref.clear(),
        ] {
            let mut c = claims("acme/widgets", "refs/heads/main");
            mutate(&mut c);
            assert_eq!(policy().evaluate(&c).reason, CiReason::IncompleteClaims);
        }
    }

    #[test]
    fn environment_and_workflow_constraints() {
        let p = CiPolicy::new(ISSUER, "acme/widgets").unwrap().with_rule(
            CiRule::new("refs/heads/main", [CiAction::UploadIndexBundle])
                .unwrap()
                .environment("production")
                .unwrap()
                .workflow("index.yml")
                .unwrap(),
        );
        let mut c = claims("acme/widgets", "refs/heads/main");
        assert!(
            !p.permits(&c, CiAction::UploadIndexBundle),
            "no environment"
        );
        c.environment = Some("staging".to_owned());
        assert!(
            !p.permits(&c, CiAction::UploadIndexBundle),
            "wrong environment"
        );
        c.environment = Some("production".to_owned());
        assert!(p.permits(&c, CiAction::UploadIndexBundle));
        c.workflow = "other.yml".to_owned();
        assert!(
            !p.permits(&c, CiAction::UploadIndexBundle),
            "wrong workflow"
        );
    }

    #[test]
    fn release_branch_wildcard() {
        let p = CiPolicy::new(ISSUER, "acme/widgets").unwrap().with_rule(
            CiRule::new("refs/heads/release/*", [CiAction::TriggerIndexUpdate]).unwrap(),
        );
        assert!(p.permits(
            &claims("acme/widgets", "refs/heads/release/1.2"),
            CiAction::TriggerIndexUpdate
        ));
        assert!(!p.permits(
            &claims("acme/widgets", "refs/heads/release"),
            CiAction::TriggerIndexUpdate
        ));
        assert!(!p.permits(
            &claims("acme/widgets", "refs/heads/main"),
            CiAction::TriggerIndexUpdate
        ));
    }

    #[test]
    fn rule_validation() {
        for bad in [
            "refs/*",
            "*",
            "refs/pull/*",
            "refs/heads*",
            "refs/he*ads/x",
            "refs/heads/*/x*",
            "refs/heads/a*b",
            "",
        ] {
            assert!(
                CiRule::new(bad, [CiAction::ReadIndexStatus]).is_err(),
                "{bad}"
            );
        }
        assert_eq!(
            CiRule::new("refs/heads/main", []),
            Err(CiPolicyError::Empty("actions"))
        );
        assert!(CiPolicy::new("", "a/b").is_err());
        assert!(CiPolicy::new("i", "").is_err());
        let rule = CiRule::new("refs/heads/main", [CiAction::ReadIndexStatus]).unwrap();
        assert!(rule.clone().workflow("").is_err());
        assert!(rule.environment("").is_err());
    }

    #[test]
    fn empty_policy_denies_everything() {
        let p = CiPolicy::new(ISSUER, "acme/widgets").unwrap();
        let d = p.evaluate(&claims("acme/widgets", "refs/heads/main"));
        assert_eq!(d.reason, CiReason::NoRuleMatched);
        assert_eq!(CiReason::NoRuleMatched.code(), "no_rule_matched");
    }

    struct Fake;
    impl OidcVerifier for Fake {
        fn verify(&self, token: &str, _now: OffsetDateTime) -> Result<CiClaims, OidcError> {
            if token == "good" {
                Ok(claims("acme/widgets", "refs/heads/main"))
            } else {
                Err(OidcError::Rejected)
            }
        }
    }

    #[test]
    fn verifier_seam_is_object_safe_and_usable() {
        let v: &dyn OidcVerifier = &Fake;
        let now = OffsetDateTime::UNIX_EPOCH;
        let c = v.verify("good", now).unwrap();
        assert!(policy().permits(&c, CiAction::TriggerIndexUpdate));
        assert_eq!(v.verify("bad", now), Err(OidcError::Rejected));
    }
}
