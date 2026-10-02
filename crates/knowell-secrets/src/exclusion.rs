//! Path-only exclusion of sensitive files and directories.
//!
//! The decision is made from the repository-relative path **before any
//! content is read**, so a sensitive file is never opened, hashed, indexed
//! or embedded. Built-in rules are always on and there is deliberately no
//! API to remove them; configuration can only add patterns.
//!
//! # Built-in rules
//!
//! File names are matched case-insensitively (Windows and macOS file systems
//! are). Before matching, a name is cut at the first `:` (NTFS alternate data
//! streams such as `.env::$DATA`) and trailing dots and spaces are trimmed
//! (Windows ignores them, so `.env.` is `.env`). The rules are conservative
//! by design: a false exclusion costs a missing file in the index, a false
//! inclusion costs a leaked credential.
//!
//! | Kind | Matches | Why |
//! |---|---|---|
//! | `env_file` | `.env`, `.env.*`, `*.env`, `.envrc` | environment files hold live credentials; `.env.example` is excluded too because templates often get real values pasted in |
//! | `private_key` | `id_rsa*`, `id_ed25519*`, `id_ecdsa*`, `id_dsa*`, `*.pem`, `*.key`, `*.ppk`, anything under `.ssh/` or `.gnupg/` | private key material (`id_rsa.pub` is excluded as well; the prefix rule is intentionally blunt) |
//! | `keystore` | `*.p12`, `*.pfx`, `*.jks`, `*.keystore`, `*.kdbx` | binary containers of keys and passwords |
//! | `credentials` | `credentials`, `credentials.json`, `.git-credentials`, `.htpasswd`, `service-account*.json`, `secrets.{json,yml,yaml,toml}` (also covers `.aws/credentials`) | cloud and service account credentials, password hashes |
//! | `registry_auth` | `.npmrc`, `.yarnrc.yml`, `.pypirc`, `.netrc`, `_netrc`, `.docker/config.json` | package and image registry auth tokens |
//! | `infra_state` | `*.tfstate`, `*.tfstate.*`, `*.tfvars`, `*.tfvars.json`, `kubeconfig`, `.kube/config` | Terraform state and variables embed secrets in clear; kubeconfig holds cluster credentials |
//! | `mobile_config` | `google-services.json`, `GoogleService-Info.plist` | app-bound API keys and project identifiers |
//! | `vpn` | `*.ovpn` | VPN profiles embed keys and credentials |
//!
//! Directories `.git` (repository internals; also matches a `.git` gitfile)
//! and `.knowell` (engine state) are always excluded as [`Exclusion::Internal`].
//!
//! # User patterns
//!
//! [`ExclusionPolicy::with_patterns`] accepts gitignore-flavoured globs:
//! a pattern without `/` matches at any depth, a leading `/` anchors it to
//! the root, `*` and `?` do not cross `/`, `**` does, and a pattern that
//! names a directory also excludes everything below it. Patterns are
//! case-sensitive and are matched against the `/`-separated
//! [`RepoPath`].

use globset::{GlobBuilder, GlobMatcher};
use knowell_core::RepoPath;
use serde::{Deserialize, Serialize};

use crate::error::SecretsError;

/// Category of a built-in sensitive-file rule. See the module docs for the
/// exact patterns and the reason behind each.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SensitiveKind {
    /// `.env`, `.env.*`, `*.env`, `.envrc`.
    EnvFile,
    /// SSH / TLS / PGP private keys and key-named files.
    PrivateKey,
    /// Binary key stores (`*.p12`, `*.jks`, `*.kdbx`, ...).
    Keystore,
    /// Cloud / service credential files and secret manifests.
    Credentials,
    /// Package or container registry auth files.
    RegistryAuth,
    /// Terraform state / variables and kubeconfig.
    InfraState,
    /// Mobile app service configuration.
    MobileConfig,
    /// VPN profiles.
    Vpn,
}

impl SensitiveKind {
    /// Stable snake_case identifier (same as the serde form).
    pub fn as_str(self) -> &'static str {
        match self {
            Self::EnvFile => "env_file",
            Self::PrivateKey => "private_key",
            Self::Keystore => "keystore",
            Self::Credentials => "credentials",
            Self::RegistryAuth => "registry_auth",
            Self::InfraState => "infra_state",
            Self::MobileConfig => "mobile_config",
            Self::Vpn => "vpn",
        }
    }

    /// One-line human reason, safe to show in UI and reports.
    pub fn reason(self) -> &'static str {
        match self {
            Self::EnvFile => "environment files commonly hold live credentials",
            Self::PrivateKey => "private key material",
            Self::Keystore => "binary container of keys or passwords",
            Self::Credentials => "credential or secret manifest",
            Self::RegistryAuth => "registry authentication token store",
            Self::InfraState => "infrastructure state or cluster credentials",
            Self::MobileConfig => "mobile app service configuration with embedded keys",
            Self::Vpn => "VPN profile with embedded keys or credentials",
        }
    }
}

/// Why a path is excluded.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Exclusion {
    /// A built-in sensitive-file rule matched.
    Sensitive(SensitiveKind),
    /// A user pattern matched; carries the pattern as configured.
    Pattern(String),
    /// Repository or engine internals (`.git`, `.knowell`); not "sensitive"
    /// content, just never part of the project's source.
    Internal,
}

/// Directory names that are never walked, whatever their content.
const INTERNAL_DIRS: [&str; 2] = [".git", ".knowell"];

/// Directories whose entire content is key material.
fn sensitive_dir(name: &str) -> Option<SensitiveKind> {
    match name {
        ".ssh" | ".gnupg" => Some(SensitiveKind::PrivateKey),
        _ => None,
    }
}

fn normalise_name(name: &str) -> String {
    let lower = name.to_lowercase();
    let cut = lower.split(':').next().unwrap_or("");
    cut.trim_end_matches(['.', ' ']).to_owned()
}

/// Classifies a (normalised, lowercase) file name; `parent` is the
/// normalised name of the containing directory.
fn classify_file(name: &str, parent: Option<&str>) -> Option<SensitiveKind> {
    if name.is_empty() {
        return None;
    }
    let ext = name.rsplit_once('.').map(|(_, e)| e);

    if name == ".env" || name == ".envrc" || name.starts_with(".env.") || name.ends_with(".env") {
        return Some(SensitiveKind::EnvFile);
    }
    if ["id_rsa", "id_ed25519", "id_ecdsa", "id_dsa"]
        .iter()
        .any(|p| name.starts_with(p))
        || matches!(ext, Some("pem" | "key" | "ppk"))
    {
        return Some(SensitiveKind::PrivateKey);
    }
    if matches!(ext, Some("p12" | "pfx" | "jks" | "keystore" | "kdbx")) {
        return Some(SensitiveKind::Keystore);
    }
    if matches!(
        name,
        "credentials"
            | "credentials.json"
            | ".git-credentials"
            | ".htpasswd"
            | "secrets.json"
            | "secrets.yml"
            | "secrets.yaml"
            | "secrets.toml"
    ) || (name.starts_with("service-account") && name.ends_with(".json"))
    {
        return Some(SensitiveKind::Credentials);
    }
    if matches!(
        name,
        ".npmrc" | ".yarnrc.yml" | ".pypirc" | ".netrc" | "_netrc"
    ) || (name == "config.json" && parent == Some(".docker"))
    {
        return Some(SensitiveKind::RegistryAuth);
    }
    if ext == Some("tfstate")
        || name.contains(".tfstate.")
        || ext == Some("tfvars")
        || name.ends_with(".tfvars.json")
        || name == "kubeconfig"
        || (name == "config" && parent == Some(".kube"))
    {
        return Some(SensitiveKind::InfraState);
    }
    if name == "google-services.json" || name == "googleservice-info.plist" {
        return Some(SensitiveKind::MobileConfig);
    }
    if ext == Some("ovpn") {
        return Some(SensitiveKind::Vpn);
    }
    None
}

#[derive(Clone)]
struct UserPattern {
    source: String,
    matchers: Vec<GlobMatcher>,
}

impl UserPattern {
    fn compile(source: &str) -> Result<Self, SecretsError> {
        let invalid = |reason: String| SecretsError::InvalidPattern {
            pattern: source.to_owned(),
            reason,
        };
        let trimmed = source.trim();
        let body = trimmed.trim_end_matches('/');
        if body.is_empty() || body == "/" {
            return Err(invalid("pattern is empty".into()));
        }
        let anchored = body.starts_with('/') || body.contains('/');
        let body = body.trim_start_matches('/');
        let base = if anchored {
            body.to_owned()
        } else {
            format!("**/{body}")
        };
        // The pattern itself, plus everything below it when it names a directory.
        let mut matchers = Vec::with_capacity(2);
        for glob in [base.clone(), format!("{base}/**")] {
            let compiled = GlobBuilder::new(&glob)
                .literal_separator(true)
                .build()
                .map_err(|e| invalid(e.kind().to_string()))?;
            matchers.push(compiled.compile_matcher());
        }
        Ok(Self {
            source: source.to_owned(),
            matchers,
        })
    }

    fn matches(&self, path: &str) -> bool {
        self.matchers.iter().any(|m| m.is_match(path))
    }
}

/// Decides which paths must never be read.
///
/// Always contains the built-in rules; user patterns only add to them.
#[derive(Clone, Default)]
pub struct ExclusionPolicy {
    patterns: Vec<UserPattern>,
}

impl ExclusionPolicy {
    /// A policy with the built-in rules only.
    pub fn builtin() -> Self {
        Self::default()
    }

    /// Built-in rules plus the given user glob patterns (see the module
    /// docs for the syntax).
    ///
    /// # Errors
    /// [`SecretsError::InvalidPattern`] for an empty or malformed glob.
    pub fn with_patterns(patterns: &[String]) -> Result<Self, SecretsError> {
        let patterns = patterns
            .iter()
            .map(|p| UserPattern::compile(p))
            .collect::<Result<Vec<_>, _>>()?;
        Ok(Self { patterns })
    }

    /// Checks a file path. `None` means the file may be read.
    ///
    /// Every component is considered: `.git/config`, `.ssh/anything` and
    /// `vendor/.env` are all excluded.
    pub fn check(&self, path: &RepoPath) -> Option<Exclusion> {
        let components: Vec<&str> = path.components().collect();
        let (name, dirs) = components.split_last()?;
        if let Some(found) = check_dir_components(dirs) {
            return Some(found);
        }
        let name_norm = normalise_name(name);
        if INTERNAL_DIRS.contains(&name_norm.as_str()) {
            return Some(Exclusion::Internal);
        }
        if let Some(kind) = sensitive_dir(&name_norm) {
            return Some(Exclusion::Sensitive(kind));
        }
        let parent = dirs.last().map(|d| normalise_name(d));
        if let Some(kind) = classify_file(&name_norm, parent.as_deref()) {
            return Some(Exclusion::Sensitive(kind));
        }
        self.match_patterns(path.as_str())
    }

    /// Checks a directory so walkers can prune it without descending.
    ///
    /// Only directory-level rules apply (`.git`, `.knowell`, `.ssh`,
    /// `.gnupg`, user patterns); file-name rules are not applied to
    /// directories. Files below a pruned directory need no further check.
    pub fn check_dir(&self, dir: &RepoPath) -> Option<Exclusion> {
        let components: Vec<&str> = dir.components().collect();
        check_dir_components(&components).or_else(|| self.match_patterns(dir.as_str()))
    }

    fn match_patterns(&self, path: &str) -> Option<Exclusion> {
        self.patterns
            .iter()
            .find(|p| p.matches(path))
            .map(|p| Exclusion::Pattern(p.source.clone()))
    }
}

fn check_dir_components(components: &[&str]) -> Option<Exclusion> {
    for component in components {
        let norm = normalise_name(component);
        if INTERNAL_DIRS.contains(&norm.as_str()) {
            return Some(Exclusion::Internal);
        }
        if let Some(kind) = sensitive_dir(&norm) {
            return Some(Exclusion::Sensitive(kind));
        }
    }
    None
}

impl std::fmt::Debug for ExclusionPolicy {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ExclusionPolicy")
            .field(
                "patterns",
                &self.patterns.iter().map(|p| &p.source).collect::<Vec<_>>(),
            )
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(s: &str) -> RepoPath {
        RepoPath::new(s).unwrap()
    }

    fn kind(policy: &ExclusionPolicy, path: &str) -> Option<Exclusion> {
        policy.check(&p(path))
    }

    #[test]
    fn builtin_sensitive_files() {
        use SensitiveKind::*;
        let policy = ExclusionPolicy::builtin();
        let cases: &[(&str, SensitiveKind)] = &[
            (".env", EnvFile),
            (".env.local", EnvFile),
            (".env.example", EnvFile),
            ("app/.env.production", EnvFile),
            ("prod.env", EnvFile),
            (".envrc", EnvFile),
            ("id_rsa", PrivateKey),
            ("home/id_rsa.pub", PrivateKey),
            ("config/id_ed25519", PrivateKey),
            ("id_ecdsa_work", PrivateKey),
            ("id_dsa", PrivateKey),
            ("certs/server.pem", PrivateKey),
            ("tls/site.key", PrivateKey),
            ("putty.ppk", PrivateKey),
            ("store.p12", Keystore),
            ("store.PFX", Keystore),
            ("app.jks", Keystore),
            ("release.keystore", Keystore),
            ("vault.kdbx", Keystore),
            ("credentials", Credentials),
            (".aws/credentials", Credentials),
            ("credentials.json", Credentials),
            (".git-credentials", Credentials),
            (".htpasswd", Credentials),
            ("service-account-prod.json", Credentials),
            ("service-account.json", Credentials),
            ("secrets.json", Credentials),
            ("k8s/secrets.yml", Credentials),
            ("secrets.yaml", Credentials),
            ("secrets.toml", Credentials),
            (".npmrc", RegistryAuth),
            (".yarnrc.yml", RegistryAuth),
            (".pypirc", RegistryAuth),
            (".netrc", RegistryAuth),
            ("_netrc", RegistryAuth),
            (".docker/config.json", RegistryAuth),
            ("infra/terraform.tfstate", InfraState),
            ("terraform.tfstate.backup", InfraState),
            ("prod.tfvars", InfraState),
            ("prod.tfvars.json", InfraState),
            ("kubeconfig", InfraState),
            (".kube/config", InfraState),
            ("android/app/google-services.json", MobileConfig),
            ("ios/GoogleService-Info.plist", MobileConfig),
            ("office.ovpn", Vpn),
        ];
        for (path, want) in cases {
            assert_eq!(
                kind(&policy, path),
                Some(Exclusion::Sensitive(*want)),
                "{path}"
            );
        }
    }

    #[test]
    fn matching_is_case_insensitive() {
        let policy = ExclusionPolicy::builtin();
        for path in [
            ".ENV",
            ".Env.Local",
            "ID_RSA",
            "Server.PEM",
            "KubeConfig",
            "CREDENTIALS",
        ] {
            assert!(kind(&policy, path).is_some(), "{path}");
        }
    }

    #[test]
    fn windows_name_tricks_are_normalised() {
        let policy = ExclusionPolicy::builtin();
        for path in [
            ".env.",
            ".env ",
            ".env::$DATA",
            "a/id_rsa:stream",
            "key.pem. ",
        ] {
            assert!(kind(&policy, path).is_some(), "{path}");
        }
    }

    #[test]
    fn ordinary_files_are_allowed() {
        let policy = ExclusionPolicy::builtin();
        for path in [
            "src/lib.rs",
            "README.md",
            "environment.ts",
            "src/env.rs",
            "env/config.toml",
            "docs/keys.md",
            "keyboard.ts",
            "src/config.json",
            ".docker/compose.yml",
            "config",
            ".github/workflows/ci.yml",
            "secrets.rs",
            "package.json",
            "monkey.rs",
            "credentials.rs",
            "terraform/main.tf",
            "pemfile.txt",
        ] {
            assert_eq!(kind(&policy, path), None, "{path}");
        }
    }

    #[test]
    fn internal_dirs_are_always_excluded() {
        let policy = ExclusionPolicy::builtin();
        for path in [
            ".git/config",
            "sub/.git/HEAD",
            ".knowell/index/x",
            ".GIT/x",
            ".git",
        ] {
            assert_eq!(kind(&policy, path), Some(Exclusion::Internal), "{path}");
        }
        assert_eq!(policy.check_dir(&p(".git")), Some(Exclusion::Internal));
        assert_eq!(
            policy.check_dir(&p("a/.knowell")),
            Some(Exclusion::Internal)
        );
        assert_eq!(policy.check_dir(&p("src")), None);
        // `.github` is not `.git`.
        assert_eq!(policy.check_dir(&p(".github")), None);
    }

    #[test]
    fn key_directories_are_pruned() {
        let policy = ExclusionPolicy::builtin();
        assert_eq!(
            policy.check_dir(&p("home/.ssh")),
            Some(Exclusion::Sensitive(SensitiveKind::PrivateKey))
        );
        assert_eq!(
            kind(&policy, ".gnupg/pubring.kbx"),
            Some(Exclusion::Sensitive(SensitiveKind::PrivateKey))
        );
        // File-name rules do not apply to directories.
        assert_eq!(policy.check_dir(&p("secrets.json")), None);
    }

    #[test]
    fn user_patterns_add_exclusions() {
        let policy = ExclusionPolicy::with_patterns(&[
            "*.log".into(),
            "/build/".into(),
            "docs/internal/**".into(),
            "fixtures".into(),
        ])
        .unwrap();
        assert_eq!(
            kind(&policy, "a/b/run.log"),
            Some(Exclusion::Pattern("*.log".into()))
        );
        assert_eq!(
            kind(&policy, "build/out.js"),
            Some(Exclusion::Pattern("/build/".into()))
        );
        assert_eq!(kind(&policy, "src/build/out.js"), None, "anchored");
        assert!(kind(&policy, "docs/internal/x/y.md").is_some());
        assert_eq!(kind(&policy, "docs/public/x.md"), None);
        assert!(kind(&policy, "a/fixtures/b.txt").is_some());
        assert_eq!(
            policy.check_dir(&p("a/fixtures")),
            Some(Exclusion::Pattern("fixtures".into()))
        );
        assert_eq!(kind(&policy, "src/lib.rs"), None);
    }

    #[test]
    fn star_does_not_cross_slash_in_anchored_patterns() {
        let policy = ExclusionPolicy::with_patterns(&["src/*.rs".into()]).unwrap();
        assert!(kind(&policy, "src/a.rs").is_some());
        assert_eq!(kind(&policy, "src/deep/a.rs"), None);
    }

    #[test]
    fn builtin_rules_win_and_cannot_be_overridden() {
        // Patterns are additive: nothing in the API can allow a sensitive path.
        let policy = ExclusionPolicy::with_patterns(&["!.env".into(), "unrelated".into()]).unwrap();
        assert_eq!(
            kind(&policy, ".env"),
            Some(Exclusion::Sensitive(SensitiveKind::EnvFile))
        );
    }

    #[test]
    fn invalid_patterns_are_rejected() {
        for bad in ["", "   ", "/", "a[", "{a,b"] {
            let err = ExclusionPolicy::with_patterns(&[bad.to_owned()]).unwrap_err();
            assert!(
                matches!(err, SecretsError::InvalidPattern { .. }),
                "{bad:?}"
            );
        }
    }

    #[test]
    fn identifiers_are_stable() {
        assert_eq!(SensitiveKind::EnvFile.as_str(), "env_file");
        assert_eq!(SensitiveKind::InfraState.as_str(), "infra_state");
        assert!(!SensitiveKind::Vpn.reason().is_empty());
    }
}
