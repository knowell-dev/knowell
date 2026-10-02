//! Key normalisers: turn the raw text a rule rendered into the canonical key
//! of a contract, so independent producers and consumers agree on it.
//!
//! Runtime-dynamic parts arrive as [`DYN`] and leave as `{}` with
//! [`Normalized::dynamic`] set. Declared route parameters (`:id`, `{id}`,
//! `<id>`, `<int:id>`, `[id]`) also become `{}` but are not dynamic: they are
//! part of the declared contract, not a guess.

use crate::model::DYN;

/// Name of a normaliser as used in pack files.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub(crate) enum Normalizer {
    /// `METHOD /path` for HTTP endpoints.
    Http,
    /// Event topics, queues, subjects and channels.
    Topic,
    /// Environment variable names.
    Env,
    /// i18n message keys.
    I18n,
    /// Database tables.
    Table,
    /// RPC methods (`package.Service/Method`).
    Rpc,
    /// Trimmed text, dynamic parts as `{}`.
    Plain,
}

impl Normalizer {
    /// Parses a pack-file name (`http`, `topic`, `env`, `i18n`, `table`,
    /// `rpc`, `plain`).
    pub(crate) fn parse(text: &str) -> Option<Self> {
        Some(match text {
            "http" => Self::Http,
            "topic" => Self::Topic,
            "env" => Self::Env,
            "i18n" => Self::I18n,
            "table" => Self::Table,
            "rpc" => Self::Rpc,
            "plain" => Self::Plain,
            _ => return None,
        })
    }
}

/// A normalised key.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Normalized {
    /// Canonical key.
    pub(crate) key: String,
    /// The key contains runtime-dynamic parts.
    pub(crate) dynamic: bool,
    /// Nothing literal is left: the key cannot identify a contract.
    pub(crate) unresolved: bool,
    /// Host of an absolute URL (HTTP only).
    pub(crate) host: Option<String>,
}

/// HTTP methods recognised as the first token of an endpoint key.
const METHODS: &[&str] = &[
    "GET", "POST", "PUT", "PATCH", "DELETE", "HEAD", "OPTIONS", "TRACE", "CONNECT", "*", "ALL",
    "ANY",
];

/// `ALL` / `ANY` (Express `all`, gin `Any`) mean every method.
fn canonical_method(method: &str) -> String {
    match method {
        "ALL" | "ANY" => "*".to_owned(),
        other => other.to_owned(),
    }
}

/// Applies `normalizer` to `raw`. `default_method` is used for HTTP keys
/// without a method. `None` means the text cannot be a key of that kind
/// (for example an environment name with spaces).
pub(crate) fn normalize(
    normalizer: Normalizer,
    raw: &str,
    default_method: &str,
) -> Option<Normalized> {
    match normalizer {
        Normalizer::Http => Some(http(raw, default_method)),
        Normalizer::Topic | Normalizer::I18n | Normalizer::Plain => Some(plain(raw)),
        Normalizer::Env => env(raw),
        Normalizer::Table => table(raw),
        Normalizer::Rpc => Some(rpc(raw)),
    }
}

/// Normalises `raw` as a key of `kind`, the way extractions are keyed
/// (for callers that build contract node ids from user input, such as
/// `GET /v1/orders/:id`). `None` when the text cannot be such a key.
pub fn normalize_key(kind: knowell_graph::ContractKind, raw: &str) -> Option<String> {
    use knowell_graph::ContractKind as K;
    let normalizer = match kind {
        K::Endpoint => Normalizer::Http,
        K::Topic => Normalizer::Topic,
        K::EnvName => Normalizer::Env,
        K::I18nKey => Normalizer::I18n,
        K::Table => Normalizer::Table,
        K::Rpc => Normalizer::Rpc,
        K::Package | K::Infra => Normalizer::Plain,
    };
    normalize(normalizer, raw, "*").map(|n| n.key)
}

/// Replaces [`DYN`] runs with `{}`.
pub(crate) fn render_dynamic(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut in_dyn = false;
    for c in text.chars() {
        if c == DYN {
            if !in_dyn {
                out.push_str("{}");
            }
            in_dyn = true;
        } else {
            in_dyn = false;
            out.push(c);
        }
    }
    out
}

/// Whether `text` has a literal character that is not punctuation or `{}`.
fn has_literal(text: &str) -> bool {
    text.replace("{}", "")
        .chars()
        .any(|c| c != DYN && c.is_alphanumeric())
}

fn plain(raw: &str) -> Normalized {
    let trimmed = raw.trim();
    let dynamic = trimmed.contains(DYN);
    let key = render_dynamic(trimmed);
    Normalized {
        unresolved: !has_literal(&key),
        key,
        dynamic,
        host: None,
    }
}

fn env(raw: &str) -> Option<Normalized> {
    let normalized = plain(raw);
    let valid = normalized
        .key
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | '-' | '{' | '}'));
    (valid && !normalized.key.is_empty()).then_some(normalized)
}

fn table(raw: &str) -> Option<Normalized> {
    let cleaned: String = raw
        .trim()
        .chars()
        .filter(|c| !matches!(c, '"' | '`' | '[' | ']'))
        .collect();
    let lower = cleaned.to_lowercase();
    let mut parts: Vec<&str> = lower.split('.').filter(|p| !p.is_empty()).collect();
    if parts.len() == 2 && matches!(parts.first(), Some(&("public" | "dbo" | "main"))) {
        parts.remove(0);
    }
    let joined = parts.join(".");
    if joined.is_empty() || joined.chars().any(char::is_whitespace) {
        return None;
    }
    Some(plain(&joined))
}

fn rpc(raw: &str) -> Normalized {
    let trimmed = raw.trim().trim_start_matches('/');
    plain(trimmed)
}

/// One `/`-separated path segment of an HTTP key.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Segment {
    /// Literal text.
    Literal(String),
    /// A declared parameter or a dynamic value covering the whole segment.
    Param,
    /// Literal text mixed with dynamic parts (`orders{}`).
    Glob(String),
}

impl Segment {
    fn render(&self) -> String {
        match self {
            Self::Literal(text) | Self::Glob(text) => text.clone(),
            Self::Param => "{}".to_owned(),
        }
    }
}

fn http(raw: &str, default_method: &str) -> Normalized {
    let trimmed = raw.trim();
    let (method, rest) = match trimmed.split_once(char::is_whitespace) {
        Some((first, rest)) if METHODS.contains(&first.to_ascii_uppercase().as_str()) => {
            (canonical_method(&first.to_ascii_uppercase()), rest.trim())
        }
        _ if METHODS.contains(&trimmed.to_ascii_uppercase().as_str()) => {
            (canonical_method(&trimmed.to_ascii_uppercase()), "")
        }
        _ => (default_method.to_ascii_uppercase(), trimmed),
    };
    let mut dynamic = false;
    let mut host = None;
    let mut path = rest;

    // Scheme and host of an absolute URL.
    let lower = path.to_ascii_lowercase();
    if lower.starts_with("http://") || lower.starts_with("https://") || path.starts_with("//") {
        let after = path
            .find("//")
            .map_or(path, |i| path.get(i + 2..).unwrap_or(""));
        let (authority, remainder) = match after.find('/') {
            Some(i) => (after.get(..i).unwrap_or(""), after.get(i..).unwrap_or("")),
            None => (after, ""),
        };
        if authority.contains(DYN) {
            dynamic = true;
        } else if !authority.is_empty() {
            host = Some(authority.to_owned());
        }
        path = remainder;
    } else if path.starts_with(DYN) {
        // A dynamic base URL (`${API}/v1/...`): which service it points to
        // is unknown, so the match is heuristic.
        let stripped = path.trim_start_matches(DYN);
        dynamic = true;
        if stripped.is_empty() {
            return Normalized {
                key: format!("{method} {{}}"),
                dynamic: true,
                unresolved: true,
                host,
            };
        }
        path = stripped;
    }
    let path = path.split(['?', '#']).next().unwrap_or("");
    let mut segments = Vec::new();
    for part in path.split('/').filter(|p| !p.is_empty()) {
        let (segment, is_dynamic) = segment(part);
        dynamic |= is_dynamic;
        segments.push(segment);
    }
    let rendered: Vec<String> = segments.iter().map(Segment::render).collect();
    let key = format!("{method} /{}", rendered.join("/"));
    let literal = segments
        .iter()
        .any(|s| matches!(s, Segment::Literal(_) | Segment::Glob(_)));
    Normalized {
        unresolved: dynamic && !literal,
        key,
        dynamic,
        host,
    }
}

fn segment(part: &str) -> (Segment, bool) {
    if part.chars().all(|c| c == DYN) {
        return (Segment::Param, true);
    }
    let declared = (part.starts_with(':') && part.len() > 1)
        || (part.starts_with('{') && part.ends_with('}'))
        || (part.starts_with('<') && part.ends_with('>'))
        || (part.starts_with('[') && part.ends_with(']'))
        || part == "*"
        || part == "**";
    if declared {
        return (Segment::Param, false);
    }
    if part.contains(DYN) {
        return (Segment::Glob(render_dynamic(part)), true);
    }
    if part.contains('{') && part.contains('}') {
        // A parameter inside a segment (`file.{ext}`): keep the literal text
        // and match the parameter like a dynamic part.
        return (Segment::Glob(collapse_braces(part)), false);
    }
    (Segment::Literal(part.to_owned()), false)
}

/// `a{x}b` -> `a{}b`.
fn collapse_braces(part: &str) -> String {
    let mut out = String::new();
    let mut depth = 0usize;
    for c in part.chars() {
        match c {
            '{' => {
                if depth == 0 {
                    out.push_str("{}");
                }
                depth += 1;
            }
            '}' if depth > 0 => depth -= 1,
            _ if depth == 0 => out.push(c),
            _ => {}
        }
    }
    out
}

/// Splits an endpoint key into method and segments.
pub(crate) fn endpoint_parts(key: &str) -> (String, Vec<Segment>) {
    let (method, path) = key.split_once(' ').unwrap_or(("*", key));
    let segments = path
        .split('/')
        .filter(|p| !p.is_empty())
        .map(|p| {
            if p == "{}" {
                Segment::Param
            } else if p.contains("{}") {
                Segment::Glob(p.to_owned())
            } else {
                Segment::Literal(p.to_owned())
            }
        })
        .collect();
    (method.to_owned(), segments)
}

/// Matches `text` against a pattern in which `{}` stands for any (possibly
/// empty) run of characters.
pub(crate) fn glob_match(pattern: &str, text: &str) -> bool {
    let parts: Vec<&str> = pattern.split("{}").collect();
    if parts.len() == 1 {
        return pattern == text;
    }
    let mut rest = text;
    let last_index = parts.len().saturating_sub(1);
    for (index, part) in parts.iter().enumerate() {
        if index == 0 {
            match rest.strip_prefix(part) {
                Some(after) => rest = after,
                None => return false,
            }
        } else if index == last_index {
            return rest.ends_with(part);
        } else if let Some(found) = rest.find(part) {
            rest = rest.get(found + part.len()..).unwrap_or("");
        } else {
            return false;
        }
    }
    true
}

/// Lower-cases and strips separators, for fuzzy name comparison
/// (`subscription_id`, `SubscriptionID` and `subscriptionId` agree).
pub(crate) fn fold_name(text: &str) -> String {
    text.chars()
        .filter(|c| c.is_alphanumeric())
        .flat_map(char::to_lowercase)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn http_key(raw: &str) -> Normalized {
        http(raw, "GET")
    }

    #[test]
    fn http_parameters_and_slashes() {
        for raw in [
            "GET /v1/orders/:id/",
            "GET v1/orders/{orderId}",
            "get /v1//orders/<int:id>",
            "GET /v1/orders/[orderId]",
        ] {
            let n = http_key(raw);
            assert_eq!(n.key, "GET /v1/orders/{}", "{raw}");
            assert!(!n.dynamic, "{raw}");
        }
    }

    #[test]
    fn http_dynamic_parts() {
        let n = http_key("POST \u{1}/v1/subscriptions/\u{1}/cancel");
        assert_eq!(n.key, "POST /v1/subscriptions/{}/cancel");
        assert!(n.dynamic);
        assert!(!n.unresolved);
        let n = http_key("GET /v1/orders\u{1}");
        assert_eq!(n.key, "GET /v1/orders{}");
        let n = http_key("POST \u{1}\u{1}");
        assert_eq!(n.key, "POST {}");
        assert!(n.unresolved);
        let n = http_key("\u{1}");
        assert!(n.unresolved);
    }

    #[test]
    fn http_absolute_urls_and_methods() {
        let n = http_key("https://api.example.com/v1/x?y=1#z");
        assert_eq!(n.key, "GET /v1/x");
        assert_eq!(n.host.as_deref(), Some("api.example.com"));
        assert_eq!(http("/healthz", "*").key, "* /healthz");
        assert_eq!(http_key("delete /").key, "DELETE /");
        assert_eq!(http_key("POST").key, "POST /");
        assert_eq!(http_key("ALL /v1/ping").key, "* /v1/ping");
        assert_eq!(http_key("/v1/file.{ext}").key, "GET /v1/file.{}");
        // `//host/path` is a protocol-relative URL.
        let n = http_key("//cdn.example.com/v1/x");
        assert_eq!(n.key, "GET /v1/x");
        assert_eq!(n.host.as_deref(), Some("cdn.example.com"));
    }

    #[test]
    fn tables_env_and_plain() {
        let t = table("public.\"Bays\"").unwrap();
        assert_eq!(t.key, "bays");
        assert_eq!(table("audit.events").unwrap().key, "audit.events");
        assert!(table("two words").is_none());
        assert!(env("NOT VALID").is_none());
        assert_eq!(env(" PORT ").unwrap().key, "PORT");
        let e = env("\u{1}").unwrap();
        assert!(e.unresolved);
        let p = plain("orders.status.\u{1}");
        assert_eq!(p.key, "orders.status.{}");
        assert!(p.dynamic && !p.unresolved);
        assert_eq!(rpc("/pkg.Svc/Get").key, "pkg.Svc/Get");
    }

    #[test]
    fn globbing() {
        assert!(glob_match("orders.status.{}", "orders.status.paid"));
        assert!(glob_match("orders{}", "orders"));
        assert!(glob_match("{}/GetRoute", "a.b.RouteService/GetRoute"));
        assert!(!glob_match("{}/GetRoute", "a.b.RouteService/GetRoutes"));
        assert!(glob_match("a{}c{}e", "abcde"));
        assert!(!glob_match("a{}c", "ab"));
        assert!(glob_match("x", "x"));
    }

    #[test]
    fn fold() {
        assert_eq!(fold_name("Subscription_ID"), "subscriptionid");
        assert_eq!(fold_name("subscription.cancelled"), "subscriptioncancelled");
    }

    #[test]
    fn endpoint_parts_roundtrip() {
        let (m, segs) = endpoint_parts("GET /v1/{}/x{}");
        assert_eq!(m, "GET");
        assert_eq!(
            segs,
            vec![
                Segment::Literal("v1".into()),
                Segment::Param,
                Segment::Glob("x{}".into())
            ]
        );
    }
}
