//! Content scanning and redaction of files that passed path exclusion.
//!
//! [`scan`] finds secret-shaped spans, [`redact`] replaces each with
//! `[REDACTED:<kind>]`. A [`Finding`] stores *where* and *what kind*, never
//! the secret text, so findings can be logged and stored freely.
//!
//! # Rules
//!
//! Specific, low-false-positive rules: PEM private key blocks, AWS access
//! key ids (`AKIA`/`ASIA`), GitHub tokens, Google API keys, Slack tokens,
//! Stripe live keys, Anthropic and OpenAI style keys, JWTs, and passwords
//! embedded in URLs (`scheme://user:PASSWORD@host`, only the password is
//! redacted). Plus a generic rule: a name containing `api key`, `secret`,
//! `token`, `password`, `client secret` or `access key`, then `:` / `=` /
//! `:=` / `=>`, then a quoted or bare value of at least 12 characters with a
//! Shannon entropy of at least 3.0 bits per character. Only the value is
//! redacted.
//!
//! The generic rule skips obvious non-secrets: environment lookups
//! (`process.env.X`, `os.getenv(..)`), interpolation (`${X}`, `{{x}}`),
//! placeholders (`<your-token-here>`, `changeme`, `xxxxxxxxxxxx`),
//! already-redacted markers, URLs and paths, code expressions, prose, and
//! values made only of letters and `_-.` (identifiers).
//!
//! # Known limits
//!
//! - **False negatives**: secrets in formats without a known prefix and
//!   without a telling variable name; low-entropy passwords (below 3.0 bits
//!   per character) and all-letter passphrases such as `correct-horse-staple`
//!   in the generic rule; values shorter than 12 characters; secrets split
//!   across lines or built by concatenation; encoded (base64 / hex) secrets
//!   without a telling name; a lone `-----END ... PRIVATE KEY-----` whose
//!   `BEGIN` is outside the scanned text; multi-line quoted values (only the
//!   first line is considered).
//! - **False positives**: hash digests or random identifiers assigned to
//!   names like `token`; the text of a `BEGIN ... PRIVATE KEY` header in
//!   documentation (the header and up to the end of the following
//!   base64-looking run are redacted). Over-redaction is the intended bias.
//! - Escapes are not decoded: `AKIA...` is not recognised.
//!
//! Scanning is linear in the input size (the `regex` engine has no
//! backtracking) apart from sorting findings.

use std::collections::BTreeMap;
use std::sync::LazyLock;

use regex::Regex;
use serde::{Deserialize, Serialize};

/// Kind of secret a [`Finding`] represents. Serialises as snake_case and is
/// the tag used inside `[REDACTED:<kind>]`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FindingKind {
    /// A PEM private key block (`-----BEGIN ... PRIVATE KEY-----`).
    PrivateKey,
    /// AWS access key id (`AKIA` / `ASIA` + 16 characters).
    AwsAccessKeyId,
    /// GitHub token (`ghp_`, `gho_`, `ghu_`, `ghs_`, `ghr_`, `github_pat_`).
    GithubToken,
    /// Google API key (`AIza` + 35 characters).
    GoogleApiKey,
    /// Slack token (`xoxa-`, `xoxb-`, `xoxp-`, `xoxr-`, `xoxs-`).
    SlackToken,
    /// Stripe live secret or restricted key (`sk_live_`, `rk_live_`).
    StripeSecret,
    /// OpenAI style key (`sk-...`).
    OpenaiKey,
    /// Anthropic style key (`sk-ant-...`).
    AnthropicKey,
    /// JSON Web Token.
    Jwt,
    /// Password embedded in a URL; the span is the password only.
    UrlPassword,
    /// High-entropy value assigned to a secret-looking name.
    GenericSecret,
}

impl FindingKind {
    /// Stable snake_case identifier, identical to the serde form.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::PrivateKey => "private_key",
            Self::AwsAccessKeyId => "aws_access_key_id",
            Self::GithubToken => "github_token",
            Self::GoogleApiKey => "google_api_key",
            Self::SlackToken => "slack_token",
            Self::StripeSecret => "stripe_secret",
            Self::OpenaiKey => "openai_key",
            Self::AnthropicKey => "anthropic_key",
            Self::Jwt => "jwt",
            Self::UrlPassword => "url_password",
            Self::GenericSecret => "generic_secret",
        }
    }
}

/// A located secret. Never contains the secret text.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Finding {
    /// What was found.
    pub kind: FindingKind,
    /// 1-based line on which the secret starts (lines end at `\n`).
    pub line: u32,
    /// Byte offset of the first byte of the secret in the scanned text.
    pub start: usize,
    /// Byte offset one past the last byte of the secret (exclusive).
    pub end: usize,
}

/// Result of [`redact`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Redacted {
    /// The text with every finding replaced by `[REDACTED:<kind>]`.
    pub text: String,
    /// The findings; offsets refer to the **original** text.
    pub findings: Vec<Finding>,
}

#[derive(Debug, Clone, Copy)]
struct Candidate {
    kind: FindingKind,
    start: usize,
    end: usize,
    /// Tie-break between equal-length overlapping candidates: lower wins.
    rank: usize,
}

const MIN_GENERIC_LEN: usize = 12;
const MIN_GENERIC_ENTROPY: f64 = 3.0;
/// How far past a private key header the matching footer is searched.
const MAX_KEY_WINDOW: usize = 64 * 1024;

/// Shannon entropy of `s` in bits per character (0.0 for an empty string).
pub fn shannon_entropy(s: &str) -> f64 {
    // Ordered map: float summation order must not depend on a random hasher.
    let mut counts: BTreeMap<char, usize> = BTreeMap::new();
    let mut total = 0usize;
    for c in s.chars() {
        *counts.entry(c).or_insert(0) += 1;
        total += 1;
    }
    if total == 0 {
        return 0.0;
    }
    let total = total as f64;
    counts
        .values()
        .map(|&n| {
            let p = n as f64 / total;
            -p * p.log2()
        })
        .sum()
}

struct TokenRule {
    kind: FindingKind,
    regex: Regex,
    group: usize,
    accept: fn(&str) -> bool,
}

fn accept_all(_: &str) -> bool {
    true
}

/// OpenAI-style bodies need a digit and an uppercase letter; this keeps
/// hyphenated identifiers such as `sk-some-long-css-class-name-here` out.
fn accept_openai(token: &str) -> bool {
    token.chars().any(|c| c.is_ascii_digit()) && token.chars().any(|c| c.is_ascii_uppercase())
}

fn accept_url_password(password: &str) -> bool {
    !is_placeholder(password) && !password.starts_with(['$', '{', '<', '%'])
}

/// (kind, pattern, capture group holding the secret, acceptance check).
type RuleSpec = (FindingKind, &'static str, usize, fn(&str) -> bool);

const TOKEN_RULE_SPECS: [RuleSpec; 9] = [
    (
        FindingKind::AwsAccessKeyId,
        r"\b(?:AKIA|ASIA)[A-Z0-9]{16}\b",
        0,
        accept_all,
    ),
    (
        FindingKind::GithubToken,
        r"(?:gh[pousr]_[A-Za-z0-9]{36,255}|github_pat_[A-Za-z0-9_]{22,255})",
        0,
        accept_all,
    ),
    (
        FindingKind::GoogleApiKey,
        r"AIza[0-9A-Za-z_\-]{35}",
        0,
        accept_all,
    ),
    (
        FindingKind::SlackToken,
        r"xox[abprs]-[A-Za-z0-9\-]{10,}",
        0,
        accept_all,
    ),
    (
        FindingKind::StripeSecret,
        r"[rs]k_live_[A-Za-z0-9]{16,}",
        0,
        accept_all,
    ),
    (
        FindingKind::AnthropicKey,
        r"sk-ant-[A-Za-z0-9_\-]{20,}",
        0,
        accept_all,
    ),
    (
        FindingKind::OpenaiKey,
        r"\bsk-(?:proj-|svcacct-)?[A-Za-z0-9_\-]{32,}",
        0,
        accept_openai,
    ),
    (
        FindingKind::Jwt,
        r"eyJ[A-Za-z0-9_\-]{8,}\.eyJ[A-Za-z0-9_\-]{8,}\.[A-Za-z0-9_\-]*",
        0,
        accept_all,
    ),
    (
        FindingKind::UrlPassword,
        r#"[A-Za-z][A-Za-z0-9+.\-]*://[^\s/:@'"<>]+:([^\s/@'"<>]+)@"#,
        1,
        accept_url_password,
    ),
];

/// Number of token rules; asserted in tests so a pattern that fails to
/// compile cannot silently disable a rule.
#[cfg(test)]
const EXPECTED_TOKEN_RULES: usize = TOKEN_RULE_SPECS.len();

static TOKEN_RULES: LazyLock<Vec<TokenRule>> = LazyLock::new(|| {
    TOKEN_RULE_SPECS
        .iter()
        .filter_map(|&(kind, pattern, group, accept)| {
            Regex::new(pattern).ok().map(|regex| TokenRule {
                kind,
                regex,
                group,
                accept,
            })
        })
        .collect()
});

static KEY_HEADER: LazyLock<Option<Regex>> =
    LazyLock::new(|| Regex::new(r"-----BEGIN [A-Z0-9 ]*PRIVATE KEY-----").ok());
static KEY_FOOTER: LazyLock<Option<Regex>> =
    LazyLock::new(|| Regex::new(r"-----END [A-Z0-9 ]*PRIVATE KEY-----").ok());

/// Groups: 1 = name, 2 = double-quoted value, 3 = single-quoted value,
/// 4 = bare value.
static GENERIC: LazyLock<Option<Regex>> = LazyLock::new(|| {
    Regex::new(concat!(
        r#"(?i)([a-z0-9_.\-]*(?:api[_\-]?key|secret|token|passw(?:or)?d|client[_\-]?secret|access[_\-]?key)[a-z0-9_\-]{0,12})"#,
        r#"["']?[ \t]*(?:=>|:=|[:=])[ \t]*"#,
        r#"(?:"((?:[^"\\\n]|\\.){12,})""#,
        r#"|'((?:[^'\\\n]|\\.){12,})'"#,
        r#"|([^\s"'`,;#=<>(){}\[\]$][^\s"'`,;#]{11,}))"#,
    ))
    .ok()
});

/// Name suffixes that make a "secret-looking" name harmless: the value is a
/// location, a label or a number, not a credential.
const BENIGN_NAME_SUFFIXES: [&str; 22] = [
    "url",
    "uri",
    "endpoint",
    "path",
    "file",
    "name",
    "field",
    "type",
    "header",
    "prefix",
    "label",
    "ttl",
    "expiry",
    "expires",
    "expiration",
    "length",
    "regex",
    "pattern",
    "env",
    "var",
    "ref",
    "count",
];

const PLACEHOLDER_MARKERS: [&str; 17] = [
    "changeme",
    "change-me",
    "change_me",
    "your",
    "example",
    "placeholder",
    "redacted",
    "dummy",
    "sample",
    "todo",
    "fixme",
    "insert",
    "replace",
    "xxxx",
    "****",
    "<",
    "secret-here",
];

const CODE_MARKERS: [&str; 9] = [
    "process.env",
    "import.meta.env",
    "getenv",
    "os.environ",
    "env::var",
    "environ[",
    "env[",
    "${",
    "{{",
];

fn is_placeholder(value: &str) -> bool {
    let lower = value.to_lowercase();
    if lower.is_empty() {
        return true;
    }
    if lower
        .chars()
        .all(|c| matches!(c, 'x' | '*' | '.' | '#' | '-' | '_' | '0' | '\u{2022}'))
    {
        return true;
    }
    let mut chars = lower.chars();
    if let Some(first) = chars.next()
        && chars.all(|c| c == first)
    {
        return true;
    }
    if matches!(
        lower.as_str(),
        "password"
            | "passwd"
            | "secret"
            | "token"
            | "apikey"
            | "api_key"
            | "null"
            | "none"
            | "undefined"
            | "string"
            | "empty"
            | "true"
            | "false"
    ) {
        return true;
    }
    PLACEHOLDER_MARKERS.iter().any(|m| lower.contains(m))
}

fn looks_like_code(value: &str) -> bool {
    if value.contains(['(', ')']) || value.starts_with(['$', '%']) || value.contains("%(") {
        return true;
    }
    let lower = value.to_lowercase();
    if CODE_MARKERS.iter().any(|m| lower.contains(m)) {
        return true;
    }
    is_dotted_identifier(value)
}

/// `this.config.apiKey`, `settings.SECRET_2`: attribute access, not a secret.
fn is_dotted_identifier(value: &str) -> bool {
    let mut parts = 0usize;
    for part in value.split('.') {
        let mut chars = part.chars();
        let Some(first) = chars.next() else {
            return false;
        };
        if !(first.is_alphabetic() || first == '_' || first == '$')
            || !chars.all(|c| c.is_alphanumeric() || c == '_' || c == '$')
        {
            return false;
        }
        parts += 1;
    }
    parts >= 2
}

fn is_location(value: &str) -> bool {
    let lower = value.to_ascii_lowercase();
    ["http://", "https://", "/", "./", "../", "~/"]
        .iter()
        .any(|p| lower.starts_with(p))
}

/// Only letters and `_-.`: an identifier or a word list, not a random secret.
fn is_identifier_like(value: &str) -> bool {
    value
        .chars()
        .all(|c| c.is_ascii_alphabetic() || matches!(c, '_' | '-' | '.'))
}

fn generic_value_ok(name: &str, value: &str, quoted: bool) -> bool {
    let name = name.to_ascii_lowercase();
    if BENIGN_NAME_SUFFIXES.iter().any(|s| name.ends_with(s)) {
        return false;
    }
    if value.chars().count() < MIN_GENERIC_LEN
        || is_placeholder(value)
        || looks_like_code(value)
        || is_location(value)
        || is_identifier_like(value)
    {
        return false;
    }
    // Prose such as "please enter your password here" is quoted text, not a secret.
    if quoted && value.chars().filter(|c| c.is_whitespace()).count() >= 2 {
        return false;
    }
    shannon_entropy(value) >= MIN_GENERIC_ENTROPY
}

fn floor_char_boundary(text: &str, mut index: usize) -> usize {
    index = index.min(text.len());
    while index > 0 && !text.is_char_boundary(index) {
        index -= 1;
    }
    index
}

/// End of an unterminated key body: the run of base64-ish characters
/// (alphabet, `+/=`, key header punctuation, whitespace) after the header.
fn unterminated_key_end(text: &str, body_start: usize) -> usize {
    let Some(rest) = text.get(body_start..) else {
        return text.len();
    };
    let stop = rest
        .char_indices()
        .find(|&(_, c)| {
            !(c.is_ascii_alphanumeric()
                || matches!(c, '+' | '/' | '=' | ':' | ',' | '-')
                || c.is_whitespace())
        })
        .map_or(rest.len(), |(i, _)| i);
    body_start + stop
}

fn collect_private_keys(text: &str, out: &mut Vec<Candidate>) {
    let (Some(header), Some(footer)) = (KEY_HEADER.as_ref(), KEY_FOOTER.as_ref()) else {
        return;
    };
    let mut pos = 0usize;
    while let Some(h) = header.find_at(text, pos) {
        let body_start = h.end();
        let window_end = floor_char_boundary(text, body_start.saturating_add(MAX_KEY_WINDOW));
        let end = text
            .get(body_start..window_end)
            .and_then(|window| footer.find(window))
            .map_or_else(
                || unterminated_key_end(text, body_start),
                |f| body_start + f.end(),
            );
        out.push(Candidate {
            kind: FindingKind::PrivateKey,
            start: h.start(),
            end,
            rank: 0,
        });
        pos = end.max(body_start);
    }
}

fn collect_tokens(text: &str, out: &mut Vec<Candidate>) {
    for (index, rule) in TOKEN_RULES.iter().enumerate() {
        for caps in rule.regex.captures_iter(text) {
            let Some(m) = caps.get(rule.group) else {
                continue;
            };
            if (rule.accept)(m.as_str()) {
                out.push(Candidate {
                    kind: rule.kind,
                    start: m.start(),
                    end: m.end(),
                    rank: index + 1,
                });
            }
        }
    }
}

fn collect_generic(text: &str, out: &mut Vec<Candidate>) {
    let Some(re) = GENERIC.as_ref() else {
        return;
    };
    for caps in re.captures_iter(text) {
        let Some(name) = caps.get(1) else { continue };
        let (value, quoted) = match (caps.get(2), caps.get(3), caps.get(4)) {
            (Some(m), _, _) | (None, Some(m), _) => (m, true),
            (None, None, Some(m)) => (m, false),
            _ => continue,
        };
        if generic_value_ok(name.as_str(), value.as_str(), quoted) {
            out.push(Candidate {
                kind: FindingKind::GenericSecret,
                start: value.start(),
                end: value.end(),
                rank: usize::MAX,
            });
        }
    }
}

/// Keeps the longest candidates, dropping any that overlap an accepted one.
fn select_non_overlapping(mut candidates: Vec<Candidate>) -> Vec<Candidate> {
    candidates.retain(|c| c.end > c.start);
    candidates.sort_by_key(|c| (std::cmp::Reverse(c.end - c.start), c.rank, c.start));
    let mut accepted: BTreeMap<usize, Candidate> = BTreeMap::new();
    for cand in candidates {
        let overlaps_before = accepted
            .range(..=cand.start)
            .next_back()
            .is_some_and(|(_, prev)| prev.end > cand.start);
        let overlaps_after = accepted
            .range(cand.start..)
            .next()
            .is_some_and(|(&next_start, _)| next_start < cand.end);
        if !overlaps_before && !overlaps_after {
            accepted.insert(cand.start, cand);
        }
    }
    accepted.into_values().collect()
}

/// Finds secrets in `text`.
///
/// The result is sorted by `start` and non-overlapping; where candidates
/// overlap the longest wins (ties go to the more specific rule). Never
/// stores the secret text.
pub fn scan(text: &str) -> Vec<Finding> {
    let mut candidates = Vec::new();
    collect_private_keys(text, &mut candidates);
    collect_tokens(text, &mut candidates);
    collect_generic(text, &mut candidates);
    let selected = select_non_overlapping(candidates);

    let bytes = text.as_bytes();
    let mut findings = Vec::with_capacity(selected.len());
    let mut line = 1usize;
    let mut cursor = 0usize;
    for cand in selected {
        let newlines = bytes
            .get(cursor..cand.start)
            .map_or(0, |chunk| chunk.iter().filter(|&&b| b == b'\n').count());
        line += newlines;
        cursor = cand.start;
        findings.push(Finding {
            kind: cand.kind,
            line: u32::try_from(line).unwrap_or(u32::MAX),
            start: cand.start,
            end: cand.end,
        });
    }
    findings
}

/// Replaces every finding in `text` with `[REDACTED:<kind>]`.
///
/// Spans always fall on UTF-8 character boundaries. Redacting already
/// redacted text changes nothing.
pub fn redact(text: &str) -> Redacted {
    let findings = scan(text);
    if findings.is_empty() {
        return Redacted {
            text: text.to_owned(),
            findings,
        };
    }
    let mut out = String::with_capacity(text.len());
    let mut cursor = 0usize;
    for finding in &findings {
        if let Some(kept) = text.get(cursor..finding.start) {
            out.push_str(kept);
        }
        out.push_str("[REDACTED:");
        out.push_str(finding.kind.as_str());
        out.push(']');
        cursor = finding.end;
    }
    if let Some(rest) = text.get(cursor..) {
        out.push_str(rest);
    }
    Redacted {
        text: out,
        findings,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use FindingKind::*;

    // All fake secrets are assembled at runtime so no realistic-looking
    // credential appears literally in the source.
    fn aws() -> String {
        format!("AKIA{}", "FAKE".repeat(4))
    }
    fn gh() -> String {
        format!("ghp_{}", "FAKE".repeat(9))
    }
    fn gh_pat() -> String {
        format!("github_pat_{}", "FAKE0".repeat(6))
    }
    fn google() -> String {
        format!("AIza{}{}", "FaKe".repeat(8), "FaK")
    }
    fn slack() -> String {
        format!("xoxb-{}", "0123456789-FAKE")
    }
    fn stripe() -> String {
        format!("sk_live_{}", "FAKE".repeat(6))
    }
    fn anthropic() -> String {
        format!("sk-ant-{}", "Fake1_".repeat(5))
    }
    fn openai() -> String {
        format!("sk-{}", "Fa1".repeat(14))
    }
    fn jwt() -> String {
        format!("eyJ{}.eyJ{}.{}", "FAKEFAKE", "FAKEFAKE", "FAKESIGNATURE")
    }
    fn strong() -> String {
        format!("Zk3Qm9Xv{}", "2LpT7wRb")
    }
    fn pem(label: &str) -> String {
        format!(
            "-----BEGIN {label} PRIVATE KEY-----\n{}\n-----END {label} PRIVATE KEY-----",
            "FAKEKEYBODY+/=\nMOREFAKEBODY"
        )
    }

    fn kinds(text: &str) -> Vec<FindingKind> {
        scan(text).into_iter().map(|f| f.kind).collect()
    }

    #[test]
    fn rules_all_compile() {
        assert_eq!(TOKEN_RULES.len(), EXPECTED_TOKEN_RULES);
        assert!(KEY_HEADER.is_some());
        assert!(KEY_FOOTER.is_some());
        assert!(GENERIC.is_some());
    }

    #[test]
    fn detects_each_provider_rule() {
        let cases: Vec<(String, FindingKind)> = vec![
            (aws(), AwsAccessKeyId),
            (gh(), GithubToken),
            (gh_pat(), GithubToken),
            (google(), GoogleApiKey),
            (slack(), SlackToken),
            (stripe(), StripeSecret),
            (anthropic(), AnthropicKey),
            (openai(), OpenaiKey),
            (jwt(), Jwt),
        ];
        for (token, kind) in cases {
            let text = format!("let x = \"{token}\";");
            let found = scan(&text);
            assert_eq!(found.len(), 1, "{kind:?}");
            let f = found[0];
            assert_eq!(f.kind, kind);
            assert_eq!(&text[f.start..f.end], token, "{kind:?}");
            let red = redact(&text);
            assert!(!red.text.contains(&token));
            assert!(red.text.contains(&format!("[REDACTED:{}]", kind.as_str())));
        }
    }

    #[test]
    fn github_token_prefixes() {
        for p in ["ghp", "gho", "ghu", "ghs", "ghr"] {
            let t = format!("{p}_{}", "Ab1".repeat(12));
            assert_eq!(kinds(&t), vec![GithubToken], "{p}");
        }
        assert!(kinds(&format!("ghx_{}", "Ab1".repeat(12))).is_empty());
        assert!(kinds("ghp_tooshort").is_empty());
    }

    #[test]
    fn aws_ids() {
        assert_eq!(
            kinds(&format!("ASIA{}", "FAKE".repeat(4))),
            vec![AwsAccessKeyId]
        );
        assert!(kinds("AKIAFAKE").is_empty());
        assert!(
            kinds(&format!("xAKIA{}", "FAKE".repeat(4))).is_empty(),
            "no word boundary"
        );
        assert!(
            kinds(&format!("AKIA{}9", "FAKE".repeat(4))).is_empty(),
            "too long"
        );
        assert!(
            kinds(&format!("AKIA{}", "fake".repeat(4))).is_empty(),
            "lowercase"
        );
    }

    #[test]
    fn anthropic_beats_openai_on_overlap() {
        let t = format!("sk-ant-api03-{}1A", "Fake".repeat(10));
        let found = scan(&t);
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].kind, AnthropicKey);
    }

    #[test]
    fn openai_rule_rejects_hyphenated_identifiers() {
        assert!(kinds("class sk-some-long-css-class-name-with-many-parts-here").is_empty());
        assert!(kinds("task-management-workflow-orchestration-service-name").is_empty());
    }

    #[test]
    fn url_password_redacts_only_the_password() {
        let pw = strong();
        let text = format!("DATABASE_URL=postgres://admin:{pw}@db.internal:5432/app");
        let found = scan(&text);
        let url: Vec<_> = found.iter().filter(|f| f.kind == UrlPassword).collect();
        assert_eq!(url.len(), 1);
        assert_eq!(&text[url[0].start..url[0].end], pw);
        let red = redact(&text);
        assert_eq!(
            red.text,
            "DATABASE_URL=postgres://admin:[REDACTED:url_password]@db.internal:5432/app"
        );
    }

    #[test]
    fn url_password_negatives() {
        for t in [
            "https://example.com/path",
            "https://user@example.com/",
            "postgres://user:${DB_PASSWORD}@host/db",
            "postgres://user:$PASS@host/db",
            "postgres://user:<password>@host/db",
            "postgres://user:password@host/db",
            "http://localhost:8080/x",
            "mailto:someone@example.com",
        ] {
            assert!(scan(t).is_empty(), "{t}");
        }
    }

    #[test]
    fn private_key_block_is_redacted_whole() {
        for label in ["RSA", "EC", "OPENSSH", "ENCRYPTED", ""] {
            let block = pem(label);
            let text = format!("before\n{block}\nafter");
            let red = redact(&text);
            assert_eq!(red.findings.len(), 1, "{label}");
            assert_eq!(red.findings[0].kind, PrivateKey);
            assert_eq!(red.findings[0].line, 2);
            assert_eq!(red.text, "before\n[REDACTED:private_key]\nafter");
        }
    }

    #[test]
    fn two_private_keys() {
        let text = format!("{}\n\n{}\n", pem("RSA"), pem("EC"));
        assert_eq!(kinds(&text), vec![PrivateKey, PrivateKey]);
    }

    #[test]
    fn truncated_private_key_is_still_redacted() {
        let text = "x = 1\n-----BEGIN RSA PRIVATE KEY-----\nMIIFAKEFAKEFAKE\nabcdEFGH+/=="; // gitleaks:allow (test fixture, not a secret)
        let red = redact(text);
        assert_eq!(red.text, "x = 1\n[REDACTED:private_key]");
        // A header mentioned in code only redacts the header itself.
        let code = r#"let h = "-----BEGIN PRIVATE KEY-----"; let y = 2;"#;
        let red = redact(code);
        assert_eq!(red.text, r#"let h = "[REDACTED:private_key]"; let y = 2;"#);
    }

    #[test]
    fn generic_positives() {
        let s = strong();
        for text in [
            format!("api_key = \"{s}\""),
            format!("apiKey: '{s}'"),
            format!("\"password\": \"{s}\""),
            format!("client_secret: {s}"),
            format!("AWS_SECRET_ACCESS_KEY={s}"),
            format!("const token = \"{s}\";"),
            format!("SECRET_KEY_BASE := {s}"),
            format!("passwd => \"{s}\""),
            format!("access-key: {s}"),
            format!("DB_PASSWORD='{s}'"),
        ] {
            let found = scan(&text);
            assert_eq!(found.len(), 1, "{text}");
            assert_eq!(found[0].kind, GenericSecret);
            assert_eq!(&text[found[0].start..found[0].end], s, "{text}");
            assert!(!redact(&text).text.contains(&s));
        }
    }

    #[test]
    fn generic_negatives() {
        for text in [
            "api_key = process.env.API_KEY_FOR_SERVICE",
            "const key = process.env.API_KEY;",
            "token = \"${API_KEY_FROM_ENV}\"",
            "token: ${{ secrets.GITHUB_TOKEN_VALUE }}",
            "token = os.getenv(\"TOKEN\")",
            "token = os.environ[\"SERVICE_TOKEN\"]",
            "password = \"<your-token-here>\"",
            "password = \"changeme\"",
            "password = \"changeme-please-1234\"",
            "secret = \"xxxxxxxxxxxxxxxx\"",
            "secret = \"****************\"",
            "api_key = \"your_api_key_goes_here\"",
            "token = \"{{ vault_token_value }}\"",
            "password = \"aaaaaaaaaaaaaaaa\"",
            "password = \"1111111111111111\"",
            "secret_name = \"prod/database/credentials\"",
            "token_url = \"https://example.com/oauth/token\"",
            "token_endpoint: /oauth/v2/token_exchange",
            "password_field = \"user_password_field\"",
            "password = \"please enter your password here\"",
            "token = self.config.auth_token_value",
            "api_key = settings.SERVICE_API_KEY2",
            "secret = getSecretValueFromVault",
            "token = get_token_from_cache_store",
            "token = \"short\"",
            "max_tokens = 4096",
            "password = \"[REDACTED:generic_secret]\"",
            "password = [REDACTED:generic_secret]",
            "the token is valid for one hour only",
            "// api key: see the documentation for details",
        ] {
            assert!(scan(text).is_empty(), "false positive: {text}");
        }
    }

    #[test]
    fn entropy_values() {
        assert_eq!(shannon_entropy(""), 0.0);
        assert_eq!(shannon_entropy("aaaa"), 0.0);
        assert!((shannon_entropy("abcd") - 2.0).abs() < 1e-9);
        assert!(shannon_entropy(&strong()) > 3.5);
    }

    #[test]
    fn jwt_inside_assignment_is_one_finding() {
        let text = format!("token = \"{}\"", jwt());
        let found = scan(&text);
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].kind, Jwt);
    }

    #[test]
    fn url_inside_assignment_is_one_finding() {
        let text = format!("secret = \"https://u:{}@host.example\"", strong());
        assert_eq!(kinds(&text), vec![UrlPassword]);
    }

    #[test]
    fn findings_sorted_non_overlapping_with_lines() {
        let text = format!(
            "one\ntwo {}\nthree\r\nfour {}\r\nfive\napi_key = \"{}\"\n",
            gh(),
            aws(),
            strong()
        );
        let found = scan(&text);
        assert_eq!(
            found.iter().map(|f| (f.kind, f.line)).collect::<Vec<_>>(),
            vec![(GithubToken, 2), (AwsAccessKeyId, 4), (GenericSecret, 6)]
        );
        assert!(found.windows(2).all(|w| w[0].end <= w[1].start));
    }

    #[test]
    fn crlf_and_unicode_are_safe() {
        let text = format!(
            "héllo ✓ 日本語\r\napi_key = \"{}\" // ключ 🔑\r\n{} é",
            strong(),
            gh()
        );
        let red = redact(&text);
        assert_eq!(red.findings.len(), 2);
        assert!(red.text.contains("héllo ✓ 日本語"));
        assert!(red.text.contains("// ключ 🔑"));
        assert!(red.text.ends_with(" é"));
        for f in &red.findings {
            assert!(text.is_char_boundary(f.start) && text.is_char_boundary(f.end));
        }
    }

    #[test]
    fn unicode_secret_value_is_cut_on_boundaries() {
        let text = "password = \"Zk3Qm9Xvé2LpT7wRb🔑ß\"";
        let red = redact(text);
        assert_eq!(red.text, "password = \"[REDACTED:generic_secret]\"");
    }

    #[test]
    fn empty_and_tiny_inputs() {
        assert!(scan("").is_empty());
        assert_eq!(redact("").text, "");
        assert!(scan("\n\n\n").is_empty());
        assert!(scan("=").is_empty());
        assert!(scan("password=").is_empty());
        assert!(scan("-----BEGIN").is_empty());
    }

    #[test]
    fn redaction_is_idempotent() {
        let text = format!(
            "{}\napi_key = \"{}\"\nurl=postgres://a:{}@h/d\n{} {} {}\n",
            pem("RSA"),
            strong(),
            strong(),
            gh(),
            jwt(),
            aws()
        );
        let once = redact(&text);
        assert!(!once.findings.is_empty());
        let twice = redact(&once.text);
        assert!(twice.findings.is_empty(), "{:?}", twice.findings);
        assert_eq!(twice.text, once.text);
    }

    #[test]
    fn findings_never_serialise_the_secret() {
        let text = format!("x = {}", gh());
        let f = scan(&text);
        let dbg = format!("{f:?}");
        assert!(!dbg.contains("FAKEFAKE"));
    }

    #[test]
    fn huge_inputs_are_handled() {
        // One 3 MB line without any secret.
        let line = "abcdefghij ".repeat(300_000);
        assert!(scan(&line).is_empty());
        // Many near-misses.
        let near = "api_key=short\n".repeat(100_000);
        assert!(scan(&near).is_empty());
        // Many real findings.
        let many = format!("{}\n", gh()).repeat(20_000);
        let found = scan(&many);
        assert_eq!(found.len(), 20_000);
        assert_eq!(found.last().map(|f| f.line), Some(20_000));
        // Unterminated quote with a very long tail.
        let open = format!("password = \"{}", "A1b2C3d4".repeat(200_000));
        let _ = redact(&open);
        // A header with no end and a long non-base64 tail.
        let header = format!("-----BEGIN PRIVATE KEY-----;{}", "x;".repeat(500_000)); // gitleaks:allow (test fixture, not a secret)
        assert_eq!(redact(&header).findings.len(), 1);
    }

    #[test]
    fn key_footer_beyond_window_is_treated_as_unterminated() {
        let filler = "A".repeat(MAX_KEY_WINDOW + 10);
        let text =
            format!("-----BEGIN PRIVATE KEY-----\n{filler}\n-----END PRIVATE KEY-----\nafter");
        let red = redact(&text);
        assert_eq!(red.findings.len(), 1);
        assert!(!red.text.contains("AAAA"));
    }

    #[test]
    fn adjacent_secrets_do_not_merge() {
        let text = format!("{} {}", gh(), gh());
        assert_eq!(kinds(&text), vec![GithubToken, GithubToken]);
        assert_eq!(
            redact(&text).text,
            "[REDACTED:github_token] [REDACTED:github_token]"
        );
    }

    #[test]
    fn kind_names_match_serde() {
        let all = [
            PrivateKey,
            AwsAccessKeyId,
            GithubToken,
            GoogleApiKey,
            SlackToken,
            StripeSecret,
            OpenaiKey,
            AnthropicKey,
            Jwt,
            UrlPassword,
            GenericSecret,
        ];
        let names: std::collections::BTreeSet<_> = all.iter().map(|k| k.as_str()).collect();
        assert_eq!(names.len(), all.len(), "unique");
        assert!(
            names
                .iter()
                .all(|n| n.chars().all(|c| c.is_ascii_lowercase() || c == '_'))
        );
    }
}
