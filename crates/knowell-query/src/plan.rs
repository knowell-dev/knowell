use std::collections::BTreeSet;

use knowell_core::Name;
use serde::{Deserialize, Serialize};

use crate::glossary::{Expansion, Glossary, TermStatus};
use crate::text::{clip, fold, has_source_extension, is_file_name, is_stopword};
use crate::{Component, Degradation, SourceError};

/// What a query asks for. Selects source weights and graph edges.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Intent {
    /// A specific symbol (`PaymentService.cancel`, `retry_count`).
    ExactSymbol,
    /// A file or directory (`src/billing/service.ts`, `Cargo.toml`).
    PathOrFile,
    /// An HTTP endpoint or route (`POST /v1/subscriptions/{id}`).
    Endpoint,
    /// An error message, error code or stack trace.
    ErrorTrace,
    /// How something behaves or where it happens, in natural language.
    Behavior,
    /// What depends on something / what breaks if it changes.
    Impact,
    /// Why something is the way it is: decisions and history.
    Why,
}

impl Intent {
    /// Every intent, in declaration order.
    pub const ALL: [Intent; 7] = [
        Intent::ExactSymbol,
        Intent::PathOrFile,
        Intent::Endpoint,
        Intent::ErrorTrace,
        Intent::Behavior,
        Intent::Impact,
        Intent::Why,
    ];

    /// Stable snake_case label.
    pub fn label(self) -> &'static str {
        match self {
            Intent::ExactSymbol => "exact_symbol",
            Intent::PathOrFile => "path_or_file",
            Intent::Endpoint => "endpoint",
            Intent::ErrorTrace => "error_trace",
            Intent::Behavior => "behavior",
            Intent::Impact => "impact",
            Intent::Why => "why",
        }
    }
}

/// HTTP method named in an endpoint query.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "UPPERCASE")]
pub enum HttpMethod {
    /// `GET`
    Get,
    /// `POST`
    Post,
    /// `PUT`
    Put,
    /// `PATCH`
    Patch,
    /// `DELETE`
    Delete,
    /// `HEAD`
    Head,
    /// `OPTIONS`
    Options,
}

impl HttpMethod {
    /// Parses an upper-case method name; lower-case `get` is an English word.
    fn parse(text: &str) -> Option<Self> {
        Some(match text {
            "GET" => HttpMethod::Get,
            "POST" => HttpMethod::Post,
            "PUT" => HttpMethod::Put,
            "PATCH" => HttpMethod::Patch,
            "DELETE" => HttpMethod::Delete,
            "HEAD" => HttpMethod::Head,
            "OPTIONS" => HttpMethod::Options,
            _ => return None,
        })
    }
}

/// Shape of an exact term extracted from a query.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TermKind {
    /// camelCase, PascalCase, snake_case, SCREAMING_CASE or a call (`foo()`).
    Identifier,
    /// `Foo::bar`, `a.b.c`, `Class#method`.
    QualifiedName,
    /// A file or directory path or a file name.
    Path,
    /// A URL path / HTTP route.
    Route,
    /// An error code (`E0308`, `ERR_HTTP_HEADERS_SENT`, `ECONNREFUSED`).
    ErrorCode,
    /// An exception or error type (`NullPointerException`, `TypeError`).
    ErrorType,
    /// A quoted phrase or code fragment, searched verbatim.
    Phrase,
}

/// A term the query names exactly; exact sources look these up verbatim.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExactTerm {
    /// The term as written (paths use `/`).
    pub text: String,
    /// Its shape.
    pub kind: TermKind,
}

/// One frame of a pasted stack trace.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TraceFrame {
    /// File path as printed (may be absolute; backslashes become `/`).
    pub path: String,
    /// 1-based line, when printed.
    pub line: Option<u32>,
    /// 1-based column, when printed.
    pub column: Option<u32>,
    /// Function or method, when printed.
    pub symbol: Option<String>,
}

/// How strongly a signal indicates its intent.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SignalStrength {
    /// Decides the intent according to the precedence table.
    Strong,
    /// Decides only when nothing stronger matched (or is informational).
    Weak,
}

/// One classification rule that fired; the plan keeps all of them so the
/// chosen intent is explainable.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Signal {
    /// Intent the rule points to.
    pub intent: Intent,
    /// Strength of the rule.
    pub strength: SignalStrength,
    /// Stable rule id, e.g. `camel-case`, `http-method-route`, `question-word`.
    pub rule: String,
    /// The query text that triggered it (at most 120 characters).
    pub evidence: String,
}

/// Who decided the plan's intent.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "by", rename_all = "snake_case")]
pub enum IntentDecision {
    /// The deterministic rules.
    Rules,
    /// An external classifier overrode the rules.
    Classifier {
        /// Classifier id (name and pinned version).
        classifier: String,
        /// What the rules had decided.
        rule_intent: Intent,
    },
}

/// Planning options.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlanOptions {
    /// Also expand with suggested (unapproved) glossary links. Off by default.
    #[serde(default)]
    pub include_suggested: bool,
    /// Business domain whose glossary entries apply.
    #[serde(default)]
    pub domain: Option<Name>,
}

/// The analysed query: intent, exact terms, content words and expansions.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct QueryPlan {
    /// The query as given.
    pub query: String,
    /// The decided intent.
    pub intent: Intent,
    /// Who decided it.
    pub decided_by: IntentDecision,
    /// Other intents with signals, strongest first.
    pub secondary: Vec<Intent>,
    /// Every rule that fired, in discovery order.
    pub signals: Vec<Signal>,
    /// Exact terms (identifiers, paths, routes, error codes, phrases).
    pub exact_terms: Vec<ExactTerm>,
    /// Lower-cased content words (stopwords removed) for lexical search.
    pub words: Vec<String>,
    /// Glossary expansions applied to the search.
    pub expansions: Vec<Expansion>,
    /// Glossary suggestions found but not applied (unapproved links).
    pub suggestions: Vec<Expansion>,
    /// HTTP method of an endpoint query.
    pub http_method: Option<HttpMethod>,
    /// Frames of a pasted stack trace.
    pub trace_frames: Vec<TraceFrame>,
    /// Business domain the plan was made for.
    pub domain: Option<Name>,
    /// Whether the query was longer than the analysis limits and only a
    /// prefix (or the first terms) was analysed.
    pub truncated: bool,
    /// Optional planning steps that failed (e.g. the external classifier).
    pub degraded: Vec<Degradation>,
}

impl QueryPlan {
    /// Whether the query has nothing to search for.
    pub fn is_empty(&self) -> bool {
        self.query.trim().is_empty()
    }

    /// Terms for a lexical (BM25) source: exact terms, content words and
    /// applied expansions, de-duplicated by folded form, in that order.
    pub fn lexical_terms(&self) -> Vec<String> {
        let mut seen = BTreeSet::new();
        let candidates = self
            .exact_terms
            .iter()
            .map(|t| t.text.as_str())
            .chain(self.words.iter().map(String::as_str))
            .chain(self.expansions.iter().map(|e| e.expansion.as_str()));
        candidates
            .filter(|t| seen.insert(fold(t)))
            .map(str::to_owned)
            .collect()
    }

    /// Text for a semantic source: the query, followed by applied glossary
    /// expansions in parentheses so cross-language queries meet code names.
    pub fn semantic_text(&self) -> String {
        let query = self.query.trim();
        if self.expansions.is_empty() {
            return query.to_owned();
        }
        let extra: Vec<&str> = self
            .expansions
            .iter()
            .map(|e| e.expansion.as_str())
            .collect();
        format!("{query} ({})", extra.join(", "))
    }

    /// Exact terms of one kind.
    pub fn terms_of(&self, kind: TermKind) -> impl Iterator<Item = &str> {
        self.exact_terms
            .iter()
            .filter(move |t| t.kind == kind)
            .map(|t| t.text.as_str())
    }
}

/// Optional external intent classifier (e.g. a hosted decision model).
///
/// Off by default: it runs only when passed to [`plan_with`]. It sees the
/// rule-based plan and may override the intent or abstain. A failure keeps
/// the rule intent and is reported in [`QueryPlan::degraded`].
pub trait IntentClassifier {
    /// Classifier id: name and pinned version.
    fn id(&self) -> String;

    /// Returns an intent, or `None` to abstain.
    fn classify(&self, query: &str, rule_plan: &QueryPlan) -> Result<Option<Intent>, SourceError>;
}

/// Characters of the query analysed; the rest is ignored (and flagged).
const MAX_QUERY_CHARS: usize = 16_384;
/// Whitespace tokens analysed.
const MAX_TOKENS: usize = 4_096;
const MAX_TERMS: usize = 64;
const MAX_WORDS: usize = 128;
const MAX_FRAMES: usize = 64;
const MAX_PHRASE_CHARS: usize = 200;
const EVIDENCE_CHARS: usize = 120;

/// Plans `query` with default options and no external classifier.
pub fn plan(query: &str, glossary: &Glossary) -> QueryPlan {
    plan_with(query, glossary, &PlanOptions::default(), None)
}

/// Plans `query`: classifies the intent by deterministic rules, extracts
/// exact terms and content words, expands them with the glossary, and lets
/// an optional classifier override the intent.
///
/// The rules and their precedence are documented in the crate README.
pub fn plan_with(
    query: &str,
    glossary: &Glossary,
    options: &PlanOptions,
    classifier: Option<&dyn IntentClassifier>,
) -> QueryPlan {
    let mut analysis = Analysis::default();
    let text = match query.char_indices().nth(MAX_QUERY_CHARS) {
        Some((cut, _)) => {
            analysis.truncated = true;
            query.get(..cut).unwrap_or(query)
        }
        None => query,
    };

    analyse_lines(text, &mut analysis);
    let tokens = tokenize(text, &mut analysis.truncated);
    analyse_tokens(&tokens, &mut analysis);
    analyse_phrases(&mut analysis, text.trim_end().ends_with('?'));

    let (intent, secondary) = decide(&analysis.signals);

    let mut plan = QueryPlan {
        query: query.to_owned(),
        intent,
        decided_by: IntentDecision::Rules,
        secondary,
        signals: analysis.signals,
        exact_terms: analysis.terms,
        words: analysis.words,
        expansions: Vec::new(),
        suggestions: Vec::new(),
        http_method: analysis.method,
        trace_frames: analysis.frames,
        domain: options.domain.clone(),
        truncated: analysis.truncated,
        degraded: Vec::new(),
    };
    apply_glossary(&mut plan, glossary, &analysis.folded_words, options);

    if let Some(classifier) = classifier {
        match classifier.classify(query, &plan) {
            Ok(Some(intent)) if intent != plan.intent => {
                let rule_intent = plan.intent;
                plan.secondary.retain(|i| *i != intent);
                plan.secondary.insert(0, rule_intent);
                plan.intent = intent;
                plan.decided_by = IntentDecision::Classifier {
                    classifier: classifier.id(),
                    rule_intent,
                };
            }
            Ok(_) => {}
            Err(error) => plan
                .degraded
                .push(Degradation::new(Component::Classifier, error.to_string())),
        }
    }
    plan
}

fn apply_glossary(
    plan: &mut QueryPlan,
    glossary: &Glossary,
    folded: &[String],
    options: &PlanOptions,
) {
    let mut present: BTreeSet<String> = plan.words.iter().map(|w| fold(w)).collect();
    present.extend(plan.exact_terms.iter().map(|t| fold(&t.text)));
    let mut applied = BTreeSet::new();
    let mut suggested = BTreeSet::new();
    let found = glossary.expand(folded, options.domain.as_ref());
    for expansion in found.iter().filter(|e| e.status == TermStatus::Approved) {
        let key = fold(&expansion.expansion);
        if !present.contains(&key) && applied.insert(key) {
            plan.expansions.push(expansion.clone());
        }
    }
    for expansion in found.iter().filter(|e| e.status == TermStatus::Suggested) {
        let key = fold(&expansion.expansion);
        if present.contains(&key) || applied.contains(&key) || !suggested.insert(key.clone()) {
            continue;
        }
        if options.include_suggested {
            applied.insert(key);
            plan.expansions.push(expansion.clone());
        } else {
            plan.suggestions.push(expansion.clone());
        }
    }
}

#[derive(Default)]
struct Analysis {
    signals: Vec<Signal>,
    terms: Vec<ExactTerm>,
    words: Vec<String>,
    /// Every plain (non-code) word, folded, in order — for phrase rules and the glossary.
    folded_words: Vec<String>,
    frames: Vec<TraceFrame>,
    method: Option<HttpMethod>,
    truncated: bool,
}

impl Analysis {
    fn signal(&mut self, intent: Intent, strength: SignalStrength, rule: &str, evidence: &str) {
        let known = self
            .signals
            .iter()
            .any(|s| s.intent == intent && s.rule == rule);
        if !known {
            self.signals.push(Signal {
                intent,
                strength,
                rule: rule.to_owned(),
                evidence: clip(evidence.trim(), EVIDENCE_CHARS),
            });
        }
    }

    fn term(&mut self, text: &str, kind: TermKind) {
        let text = text.trim();
        if text.is_empty() || self.terms.iter().any(|t| t.kind == kind && t.text == text) {
            return;
        }
        if self.terms.len() >= MAX_TERMS {
            self.truncated = true;
            return;
        }
        self.terms.push(ExactTerm {
            text: clip(text, MAX_PHRASE_CHARS),
            kind,
        });
    }

    fn word(&mut self, word: String) {
        if self.words.contains(&word) {
            return;
        }
        if self.words.len() >= MAX_WORDS {
            self.truncated = true;
            return;
        }
        self.words.push(word);
    }

    /// Records a frame once per (path, line, column); line rules run before
    /// token rules, so a frame seen both ways keeps the symbol the line gave.
    fn frame(&mut self, frame: TraceFrame) {
        let existing = self
            .frames
            .iter_mut()
            .find(|f| f.path == frame.path && f.line == frame.line && f.column == frame.column);
        if let Some(existing) = existing {
            if existing.symbol.is_none() {
                existing.symbol = frame.symbol;
            }
            return;
        }
        if self.frames.len() >= MAX_FRAMES {
            self.truncated = true;
            return;
        }
        self.frames.push(frame);
    }
}

// ---------------------------------------------------------------------------
// Tokenizing

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Quote {
    Backtick,
    Double,
}

#[derive(Debug)]
struct Token {
    text: String,
    quote: Option<Quote>,
}

fn closing_quote(open: char) -> Option<(char, Quote)> {
    match open {
        '`' => Some(('`', Quote::Backtick)),
        '"' => Some(('"', Quote::Double)),
        '\u{201C}' => Some(('\u{201D}', Quote::Double)),
        _ => None,
    }
}

/// Splits on whitespace, keeping balanced backtick and double-quoted spans
/// as single tokens. Unbalanced quote characters are dropped.
fn tokenize(text: &str, truncated: &mut bool) -> Vec<Token> {
    let mut tokens = Vec::new();
    let mut rest = text;
    // A closing quote missing once is missing for the rest of the text;
    // remembering it keeps hostile input (`“““…`) linear.
    let mut absent: Vec<char> = Vec::new();
    while let Some(c) = rest.chars().next() {
        if tokens.len() >= MAX_TOKENS {
            *truncated = true;
            break;
        }
        let after = rest.get(c.len_utf8()..).unwrap_or("");
        if c.is_whitespace() {
            rest = after;
            continue;
        }
        if let Some((close, quote)) = closing_quote(c) {
            let end = if absent.contains(&close) {
                None
            } else {
                after.find(close)
            };
            if end.is_none() && !absent.contains(&close) {
                absent.push(close);
            }
            if let Some(end) = end {
                let inner = after.get(..end).unwrap_or("").trim();
                if !inner.is_empty() {
                    tokens.push(Token {
                        text: inner.to_owned(),
                        quote: Some(quote),
                    });
                }
                rest = after.get(end + close.len_utf8()..).unwrap_or("");
            } else {
                rest = after;
            }
            continue;
        }
        let end = rest
            .find(|ch: char| ch.is_whitespace() || closing_quote(ch).is_some())
            .unwrap_or(rest.len());
        if end == 0 {
            rest = after;
            continue;
        }
        tokens.push(Token {
            text: rest.get(..end).unwrap_or("").to_owned(),
            quote: None,
        });
        rest = rest.get(end..).unwrap_or("");
    }
    tokens
}

const OPENERS: [(char, char); 4] = [('(', ')'), ('[', ']'), ('{', '}'), ('<', '>')];

/// Trims prose punctuation around a token without breaking code: a closing
/// bracket is removed only when it is unbalanced (`/users/{id}` keeps its
/// brace, `(see foo)` loses it).
fn clean(token: &str) -> &str {
    struct Pair {
        open: char,
        close: char,
        opens: usize,
        closes: usize,
    }
    let mut s = token;
    // Counted once and updated as characters are removed, so cleaning stays
    // linear even for hostile tokens such as `((((((…`.
    let mut pairs: Vec<Pair> = OPENERS
        .iter()
        .map(|&(open, close)| Pair {
            open,
            close,
            opens: count(s, open),
            closes: count(s, close),
        })
        .collect();
    loop {
        let before = s;
        // A token wrapped in a bracket pair: `(src/a.ts:42:7)`, `[id]`.
        let wrapped = pairs.iter_mut().find_map(|pair| {
            let inner = s.strip_prefix(pair.open)?.strip_suffix(pair.close)?;
            Some((pair, inner))
        });
        if let Some((pair, inner)) = wrapped {
            pair.opens = pair.opens.saturating_sub(1);
            pair.closes = pair.closes.saturating_sub(1);
            s = inner;
            continue;
        }
        if let Some(c) = s.chars().next() {
            let unbalanced = pairs.iter_mut().find(|p| p.open == c && p.opens > p.closes);
            let strip = matches!(c, '\'' | '\u{2018}' | ',' | ';' | '*') || unbalanced.is_some();
            if let Some(pair) = unbalanced {
                pair.opens = pair.opens.saturating_sub(1);
            }
            if strip {
                s = s.get(c.len_utf8()..).unwrap_or("");
            }
        }
        if let Some(c) = s.chars().next_back() {
            let unbalanced = pairs
                .iter_mut()
                .find(|p| p.close == c && p.closes > p.opens);
            let strip = matches!(
                c,
                ',' | ';' | ':' | '!' | '?' | '.' | '\'' | '\u{2019}' | '\u{201D}' | '*'
            ) || unbalanced.is_some();
            if let Some(pair) = unbalanced {
                pair.closes = pair.closes.saturating_sub(1);
            }
            if strip {
                s = s.get(..s.len() - c.len_utf8()).unwrap_or("");
            }
        }
        if s == before {
            return s;
        }
    }
}

fn count(s: &str, c: char) -> usize {
    s.chars().filter(|x| *x == c).count()
}

// ---------------------------------------------------------------------------
// Token shapes

#[derive(Debug, PartialEq, Eq)]
enum Shape {
    Route(String, &'static str),
    FileLocation {
        path: String,
        line: u32,
        column: Option<u32>,
    },
    Path(String, &'static str),
    ErrorCode(String),
    ExceptionType(String),
    Qualified(String),
    Identifier(String, &'static str),
}

fn is_identifier(s: &str) -> bool {
    let mut chars = s.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    (first.is_ascii_alphabetic() || first == '_' || first == '$')
        && chars.all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '$')
}

/// Identifier shape rule, if `s` looks like code rather than a word.
fn identifier_rule(s: &str) -> Option<&'static str> {
    if !is_identifier(s) || s.len() < 2 {
        return None;
    }
    let has_lower = s.chars().any(|c| c.is_ascii_lowercase());
    let uppers = s.chars().filter(char::is_ascii_uppercase).count();
    let first_lower = s.chars().next().is_some_and(|c| c.is_ascii_lowercase());
    let first_upper = s.chars().next().is_some_and(|c| c.is_ascii_uppercase());
    if s.contains('_') && s.chars().any(|c| c.is_ascii_alphanumeric()) {
        Some("snake-case")
    } else if first_lower && uppers > 0 {
        Some("camel-case")
    } else if first_upper && has_lower && uppers >= 2 {
        Some("pascal-case")
    } else {
        None
    }
}

const NOT_QUALIFIED: &[&str] = &["a.k.a", "e.g", "i.e", "vs", "etc"];
const TOP_LEVEL_DOMAINS: &[&str] = &["com", "org", "net"];

fn is_qualified(s: &str) -> bool {
    if s.contains("::") {
        let parts: Vec<&str> = s.split("::").collect();
        return parts.len() >= 2 && parts.iter().all(|p| is_identifier(p));
    }
    if let Some((owner, member)) = s.split_once('#') {
        return is_identifier(owner) && is_identifier(member);
    }
    if !s.contains('.') || (has_source_extension(s) && !is_dotted_c_name(s)) {
        return false;
    }
    let parts: Vec<&str> = s.split('.').collect();
    let last = parts
        .last()
        .map(|p| p.to_ascii_lowercase())
        .unwrap_or_default();
    parts.len() >= 2
        && parts.iter().all(|p| is_identifier(p))
        && !NOT_QUALIFIED.contains(&s.to_ascii_lowercase().as_str())
        && !TOP_LEVEL_DOMAINS.contains(&last.as_str())
}

/// `a.b.c` / `pkg.mod.h`: three or more identifier segments ending in the
/// one-letter C extensions read as a qualified name, not a file (`main.c` is
/// still a file).
fn is_dotted_c_name(s: &str) -> bool {
    let parts: Vec<&str> = s.split('.').collect();
    parts.len() >= 3
        && matches!(parts.last(), Some(&("c" | "h")))
        && parts.iter().all(|p| is_identifier(p))
}

fn last_segment(s: &str) -> &str {
    s.rsplit(['.', ':', '#']).next().unwrap_or(s)
}

fn is_exception_type(s: &str) -> bool {
    let last = last_segment(s);
    last.len() > "Exception".len()
        && last.ends_with("Exception")
        && last.chars().next().is_some_and(|c| c.is_ascii_uppercase())
        && is_identifier(last)
        && (is_identifier(s) || is_qualified(s))
}

/// `TypeError`, `ValueError`, `java.io.IOException` — accepted as an error
/// type only when followed by `:` in the query.
fn is_error_type_name(s: &str) -> bool {
    let last = last_segment(s);
    let suffix_ok = (last.ends_with("Error") && last.len() > "Error".len()) || is_exception_type(s);
    suffix_ok
        && last.chars().next().is_some_and(|c| c.is_ascii_uppercase())
        && is_identifier(last)
        && (is_identifier(s) || is_qualified(s))
}

const ERRNO_NAMES: &[&str] = &[
    "EACCES",
    "EADDRINUSE",
    "ECONNREFUSED",
    "ECONNRESET",
    "EEXIST",
    "ENOENT",
    "ENOTFOUND",
    "EPERM",
    "EPIPE",
    "ETIMEDOUT",
];

fn is_error_code(s: &str) -> bool {
    let all = |text: &str, f: fn(char) -> bool| !text.is_empty() && text.chars().all(f);
    let upper_or_digit = |c: char| c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_';
    if let Some(rest) = s.strip_prefix("ERR_") {
        return all(rest, upper_or_digit);
    }
    if let Some(rest) = s.strip_prefix('E')
        && rest.len() == 4
        && all(rest, |c| c.is_ascii_digit())
    {
        return true;
    }
    if let Some(rest) = s.strip_prefix("TS")
        && rest.len() == 4
        && all(rest, |c| c.is_ascii_digit())
    {
        return true;
    }
    if ERRNO_NAMES.contains(&s) {
        return true;
    }
    // `ORA-00942`, `SQL-30081`: 2-5 capitals, a dash, 3-5 digits.
    if let Some((prefix, digits)) = s.split_once('-') {
        return (2..=5).contains(&prefix.len())
            && all(prefix, |c| c.is_ascii_uppercase())
            && (3..=5).contains(&digits.len())
            && all(digits, |c| c.is_ascii_digit());
    }
    false
}

fn is_route_char(c: char) -> bool {
    c.is_ascii_alphanumeric()
        || matches!(
            c,
            '/' | '_' | '-' | '.' | ':' | '{' | '}' | '*' | '~' | '$' | '@' | '<' | '>'
        )
}

fn is_route(s: &str) -> bool {
    s.len() >= 2
        && s.starts_with('/')
        && !s.starts_with("//")
        && s.chars().all(is_route_char)
        && s.chars().any(|c| c.is_ascii_alphabetic())
        && !is_file_name(s.rsplit('/').next().unwrap_or(s))
}

/// The path part of an `http(s)://` URL, if it has one.
fn url_route(s: &str) -> Option<String> {
    let rest = s
        .strip_prefix("https://")
        .or_else(|| s.strip_prefix("http://"))?;
    let slash = rest.find('/')?;
    let path = rest.get(slash..)?;
    let path = path.split(['?', '#']).next().unwrap_or(path);
    is_route(path).then(|| path.to_owned())
}

/// `path:line[:column]` where the path ends in a file name.
fn parse_location(s: &str) -> Option<(String, u32, Option<u32>)> {
    let s = s.trim_end_matches([':', ',', ')']);
    let (head, last) = s.rsplit_once(':')?;
    let last: u32 = last.parse().ok()?;
    let (path, line, column) = match head.rsplit_once(':') {
        Some((path, mid)) if !mid.is_empty() && mid.chars().all(|c| c.is_ascii_digit()) => {
            (path, mid.parse().ok()?, Some(last))
        }
        _ => (head, last, None),
    };
    let path = path.replace('\\', "/");
    let name = path.rsplit('/').next().unwrap_or(&path);
    (is_file_name(name) && line > 0).then_some((path, line, column))
}

const SOURCE_DIRS: &[&str] = &[
    "app", "apps", "cmd", "config", "crates", "docs", "internal", "lib", "packages", "pkg",
    "scripts", "spec", "src", "test", "tests",
];

fn is_path_char(c: char) -> bool {
    c.is_alphanumeric() || matches!(c, '/' | '_' | '-' | '.' | '@' | '+' | '$' | '~' | '[' | ']')
}

/// A file or directory path (relative, or absolute when it ends in a file).
fn path_shape(s: &str) -> Option<String> {
    if s.contains("://") {
        return None;
    }
    let normalised = s.replace('\\', "/");
    if !normalised.contains('/') || !normalised.chars().all(is_path_char) {
        return None;
    }
    let trimmed = normalised.trim_end_matches('/');
    let segments: Vec<&str> = trimmed.split('/').filter(|p| !p.is_empty()).collect();
    let last = segments.last().copied().unwrap_or("");
    let first = segments.first().copied().unwrap_or("");
    let names_file = is_file_name(last);
    if normalised.starts_with('/') {
        return names_file.then_some(normalised);
    }
    let pathy = names_file
        || segments.len() >= 3
        || SOURCE_DIRS.contains(&first)
        || normalised.starts_with("./")
        || normalised.starts_with("../")
        || normalised.ends_with('/');
    let relative = normalised.strip_prefix("./").unwrap_or(&normalised);
    pathy.then(|| relative.to_owned())
}

fn classify(s: &str) -> Option<Shape> {
    if let Some(shape) = classify_plain(s) {
        return Some(shape);
    }
    // Call shape: `foo()`, `PaymentService.cancel(id)`.
    let (head, _) = s.split_once('(')?;
    let head = clean(head.trim());
    if let Some(shape) = classify_plain(head) {
        return Some(shape);
    }
    is_identifier(head).then(|| Shape::Identifier(head.to_owned(), "call-shape"))
}

fn classify_plain(s: &str) -> Option<Shape> {
    if s.is_empty() {
        return None;
    }
    if let Some(route) = url_route(s) {
        return Some(Shape::Route(route, "url-route"));
    }
    if let Some((path, line, column)) = parse_location(s) {
        return Some(Shape::FileLocation { path, line, column });
    }
    if is_route(s) {
        return Some(Shape::Route(s.to_owned(), "route-shape"));
    }
    if let Some(path) = path_shape(s) {
        return Some(Shape::Path(path, "path-shape"));
    }
    if is_file_name(s) && !is_dotted_c_name(s) {
        return Some(Shape::Path(s.to_owned(), "file-name"));
    }
    if is_error_code(s) {
        return Some(Shape::ErrorCode(s.to_owned()));
    }
    if is_exception_type(s) {
        return Some(Shape::ExceptionType(s.to_owned()));
    }
    if is_qualified(s) {
        return Some(Shape::Qualified(s.to_owned()));
    }
    identifier_rule(s).map(|rule| Shape::Identifier(s.to_owned(), rule))
}

fn apply_shape(shape: Shape, a: &mut Analysis) {
    use SignalStrength::Strong;
    match shape {
        Shape::Route(route, rule) => {
            a.term(&route, TermKind::Route);
            a.signal(Intent::Endpoint, Strong, rule, &route);
        }
        Shape::FileLocation { path, line, column } => {
            a.term(&path, TermKind::Path);
            a.signal(Intent::PathOrFile, Strong, "file-location", &path);
            a.frame(TraceFrame {
                path,
                line: Some(line),
                column,
                symbol: None,
            });
        }
        Shape::Path(path, rule) => {
            a.term(&path, TermKind::Path);
            a.signal(Intent::PathOrFile, Strong, rule, &path);
        }
        Shape::ErrorCode(code) => {
            a.term(&code, TermKind::ErrorCode);
            a.signal(Intent::ErrorTrace, Strong, "error-code", &code);
        }
        Shape::ExceptionType(name) => {
            a.term(&name, TermKind::ErrorType);
            a.signal(Intent::ErrorTrace, Strong, "exception-type", &name);
        }
        Shape::Qualified(name) => {
            a.term(&name, TermKind::QualifiedName);
            a.signal(Intent::ExactSymbol, Strong, "qualified-name", &name);
        }
        Shape::Identifier(name, rule) => {
            a.term(&name, TermKind::Identifier);
            a.signal(Intent::ExactSymbol, Strong, rule, &name);
        }
    }
}

/// Turkish attaches case suffixes to names with an apostrophe:
/// `PaymentService'i`, `API'nin`. Returns the part before the apostrophe.
fn strip_apostrophe_suffix(s: &str) -> Option<&str> {
    let (stem, suffix) = s.split_once(['\'', '\u{2019}'])?;
    let suffix_ok = !suffix.is_empty()
        && suffix.chars().count() <= 6
        && suffix.chars().all(char::is_alphabetic);
    (!stem.is_empty() && suffix_ok).then_some(stem)
}

fn analyse_tokens(tokens: &[Token], a: &mut Analysis) {
    let mut skip_next = false;
    for (i, token) in tokens.iter().enumerate() {
        if skip_next {
            skip_next = false;
            continue;
        }
        match token.quote {
            Some(Quote::Backtick) => analyse_code_span(&token.text, a),
            Some(Quote::Double) => a.term(&token.text, TermKind::Phrase),
            None => {
                if let Some(method) = HttpMethod::parse(&token.text)
                    && let Some(next) = tokens.get(i + 1)
                    && next.quote.is_none()
                {
                    let target = clean(&next.text);
                    let route =
                        url_route(target).or_else(|| is_route(target).then(|| target.to_owned()));
                    if let Some(route) = route {
                        a.method.get_or_insert(method);
                        a.term(&route, TermKind::Route);
                        let evidence = format!("{} {route}", token.text);
                        a.signal(
                            Intent::Endpoint,
                            SignalStrength::Strong,
                            "http-method-route",
                            &evidence,
                        );
                        skip_next = true;
                        continue;
                    }
                }
                analyse_word(&token.text, a);
            }
        }
    }
}

fn analyse_code_span(text: &str, a: &mut Analysis) {
    let cleaned = clean(text);
    if let Some(shape) = classify(cleaned) {
        apply_shape(shape, a);
    } else if is_identifier(cleaned) {
        a.term(cleaned, TermKind::Identifier);
        a.signal(
            Intent::ExactSymbol,
            SignalStrength::Strong,
            "backtick-code",
            cleaned,
        );
    } else {
        a.term(text, TermKind::Phrase);
        a.signal(
            Intent::ExactSymbol,
            SignalStrength::Strong,
            "backtick-code",
            text,
        );
    }
}

fn analyse_word(raw: &str, a: &mut Analysis) {
    let cleaned = clean(raw);
    if cleaned.is_empty() {
        return;
    }
    if raw.trim_end().ends_with(':') && is_error_type_name(cleaned) {
        a.term(cleaned, TermKind::ErrorType);
        a.signal(
            Intent::ErrorTrace,
            SignalStrength::Strong,
            "error-type-colon",
            cleaned,
        );
        return;
    }
    if let Some(shape) = classify(cleaned) {
        apply_shape(shape, a);
        return;
    }
    let stem = strip_apostrophe_suffix(cleaned);
    if let Some(stem) = stem
        && let Some(shape) = classify(stem)
    {
        apply_shape(shape, a);
        return;
    }
    let word = stem.unwrap_or(cleaned);
    let folded = fold(word);
    if !folded.chars().any(char::is_alphanumeric) {
        return;
    }
    if !is_stopword(&folded) {
        a.word(word.to_lowercase());
    }
    a.folded_words.push(folded);
}

// ---------------------------------------------------------------------------
// Line rules (stack traces and error output)

const ERROR_LINE_PREFIXES: &[&str] = &[
    "Error: ",
    "error: ",
    "ERROR: ",
    "ERROR ",
    "FATAL: ",
    "fatal: ",
    "Fatal error: ",
    "panic: ",
];

fn analyse_lines(text: &str, a: &mut Analysis) {
    use SignalStrength::Strong;
    for raw in text.lines() {
        let line = raw.trim();
        if line.is_empty() {
            continue;
        }
        if line.starts_with("Traceback (most recent call last)") {
            a.signal(Intent::ErrorTrace, Strong, "python-traceback", line);
        } else if let Some(frame) = python_frame(line) {
            a.signal(Intent::ErrorTrace, Strong, "python-frame", line);
            a.frame(frame);
        } else if let Some(rest) = line.strip_prefix("at ")
            && let Some(frame) = at_frame(rest)
        {
            a.signal(Intent::ErrorTrace, Strong, "stack-frame", line);
            a.frame(frame);
        } else if let Some(index) = line.find("panicked at") {
            a.signal(Intent::ErrorTrace, Strong, "rust-panic", line);
            let rest = line.get(index + "panicked at".len()..).unwrap_or("");
            let location = rest
                .split_whitespace()
                .find_map(|t| parse_location(t.trim_matches(['\'', ','])));
            if let Some((path, line_no, column)) = location {
                a.frame(TraceFrame {
                    path,
                    line: Some(line_no),
                    column,
                    symbol: None,
                });
            }
        } else if line.starts_with("Exception in thread")
            || line.starts_with("Caused by:")
            || line.starts_with("Uncaught ")
        {
            a.signal(Intent::ErrorTrace, Strong, "exception-header", line);
        } else if line.starts_with("goroutine ") && line.contains('[') {
            a.signal(Intent::ErrorTrace, Strong, "go-goroutine", line);
        } else if is_native_frame(line) {
            a.signal(Intent::ErrorTrace, Strong, "native-backtrace", line);
        } else if let Some(index) = line.find("error[E") {
            a.signal(Intent::ErrorTrace, Strong, "compiler-error", line);
            let code = line
                .get(index + "error[".len()..)
                .and_then(|rest| rest.split_once(']'))
                .map(|(code, _)| code);
            if let Some(code) = code
                && is_error_code(code)
            {
                a.term(code, TermKind::ErrorCode);
            }
        } else if ERROR_LINE_PREFIXES.iter().any(|p| line.starts_with(p)) {
            a.signal(Intent::ErrorTrace, Strong, "error-line", line);
        } else if let Some(first) = line.split_whitespace().next()
            && first.contains(".go:")
            && let Some((path, line_no, column)) = parse_location(first)
        {
            a.signal(Intent::ErrorTrace, Strong, "go-frame", line);
            a.frame(TraceFrame {
                path,
                line: Some(line_no),
                column,
                symbol: None,
            });
        }
    }
}

/// `File "app/x.py", line 10, in handler`
fn python_frame(line: &str) -> Option<TraceFrame> {
    let rest = line.strip_prefix("File \"")?;
    let (path, after) = rest.split_once('"')?;
    let after = after.strip_prefix(", line ")?;
    let digits: String = after.chars().take_while(char::is_ascii_digit).collect();
    let line_no: u32 = digits.parse().ok()?;
    let symbol = after
        .split_once(", in ")
        .map(|(_, s)| s.trim().to_owned())
        .filter(|s| !s.is_empty());
    Some(TraceFrame {
        path: path.replace('\\', "/"),
        line: Some(line_no),
        column: None,
        symbol,
    })
}

/// `at handler (src/app.ts:10:5)`, `at com.x.Bar.baz(Bar.java:42)`, `at src/app.ts:10:5`
fn at_frame(rest: &str) -> Option<TraceFrame> {
    if rest.ends_with(')')
        && let Some(open) = rest.rfind('(')
    {
        let inner = rest.get(open + 1..rest.len() - 1)?;
        let (path, line, column) = parse_location(inner)?;
        let symbol = rest.get(..open)?.trim();
        return Some(TraceFrame {
            path,
            line: Some(line),
            column,
            symbol: (!symbol.is_empty()).then(|| symbol.to_owned()),
        });
    }
    let (path, line, column) = parse_location(rest.trim())?;
    Some(TraceFrame {
        path,
        line: Some(line),
        column,
        symbol: None,
    })
}

/// `#3 0x00007f… in main ()` (gdb / native backtraces).
fn is_native_frame(line: &str) -> bool {
    let Some(rest) = line.strip_prefix('#') else {
        return false;
    };
    let digits = rest.chars().take_while(char::is_ascii_digit).count();
    digits > 0
        && rest
            .get(digits..)
            .is_some_and(|after| after.trim_start().starts_with("0x"))
}

// ---------------------------------------------------------------------------
// Phrase rules (question words, impact and why vocabulary, EN + TR)

const IMPACT_WORDS: &[&str] = &[
    "affected",
    "affects",
    "callers",
    "consumers",
    "dependents",
    "impact",
    "impacted",
    "impacts",
    "usages", // Turkish (folded)
    "bagimli",
    "bagimlilar",
    "bozar",
    "bozulur",
    "cagiran",
    "cagiranlar",
    "cagiriyor",
    "etki",
    "etkiler",
    "etkilenecek",
    "etkilenen",
    "etkilenir",
    "etkiliyor",
    "etkisi",
    "kirilir",
    "kullanan",
    "kullananlar",
    "kullanilan",
    "kullaniliyor",
    "kullaniyor",
];
const IMPACT_SEQUENCES: &[&[&str]] = &[
    &["blast", "radius"],
    &["breaks", "if"],
    &["callers", "of"],
    &["depend", "on"],
    &["depends", "on"],
    &["if", "i", "change"],
    &["if", "i", "delete"],
    &["if", "i", "remove"],
    &["if", "i", "rename"],
    &["if", "we", "change"],
    &["if", "we", "delete"],
    &["if", "we", "remove"],
    &["if", "we", "rename"],
    &["used", "by"],
    &["what", "breaks"],
    &["who", "calls"],
    &["who", "uses"],
    &["will", "break"],
    &["would", "break"],
];
/// Turkish conditional "if I/we …" verb endings (`silersem`, `değiştirirsek`).
const CONDITIONAL_SUFFIXES: &[&str] = &["rsak", "rsam", "rsek", "rsem"];

const WHY_WORDS: &[&str] = &[
    "adr",
    "blame",
    "decided",
    "motivation",
    "rationale",
    "reasoning",
    "why",
    // Turkish (folded)
    "gerekce",
    "gerekcesi",
    "gerekcesini",
    "neden",
    "nicin",
    "niye",
    "sebebi",
    "sebebini",
    "tarihce",
    "tarihcesi",
];
const WHY_SEQUENCES: &[&[&str]] = &[
    &["design", "decision"],
    &["git", "history"],
    &["history", "of"],
    &["reason", "for"],
    &["the", "reason"],
    &["when", "did"],
    &["when", "was"],
    &["who", "changed"],
    &["karar", "verildi"],
    &["kim", "degistirdi"],
    &["ne", "zaman"],
];
/// `neden olan` / `neden olabilir` mean "causing", not "why".
const NOT_WHY_SEQUENCES: &[&[&str]] = &[
    &["neden", "olabilecek"],
    &["neden", "olabilir"],
    &["neden", "olan"],
];

const ERROR_WORDS: &[&str] = &[
    "crash",
    "crashed",
    "crashes",
    "crashing",
    "error",
    "errors",
    "exception",
    "exceptions",
    "failing",
    "fails",
    "panic",
    "panicked",
    "panics",
    "stacktrace",
    "throws",
    "thrown",
    "traceback", // Turkish (folded)
    "cokme",
    "coktu",
    "cokuyor",
    "firlatiyor",
    "hata",
    "hatada",
    "hatalar",
    "hatalari",
    "hatanin",
    "hatasi",
    "hatasini",
    "hataya",
    "hatayi",
    "istisna",
    "patladi",
    "patliyor",
];
const ERROR_SEQUENCES: &[&[&str]] = &[&["stack", "trace"]];

const ENDPOINT_WORDS: &[&str] = &[
    "api",
    "endpoint",
    "endpoints",
    "route",
    "routes",
    "rota",
    "rotasi",
];
const ENDPOINT_SEQUENCES: &[&[&str]] = &[&["uc", "nokta"], &["uc", "noktasi"]];

const QUESTION_WORDS: &[&str] = &[
    "how", "what", "where", "which", // Turkish (folded); `mi`/`mu` are question particles
    "hangi", "hangisi", "mi", "mu", "nasil", "ne", "nedir", "nerde", "nerede", "nereden", "nereye",
];

fn contains_sequence(words: &[String], sequence: &[&str]) -> bool {
    if sequence.is_empty() {
        return false;
    }
    words
        .windows(sequence.len())
        .any(|window| window.iter().zip(sequence).all(|(w, s)| w == s))
}

fn first_word<'a>(words: &'a [String], list: &[&str]) -> Option<&'a String> {
    words.iter().find(|w| list.contains(&w.as_str()))
}

fn first_sequence(words: &[String], sequences: &[&[&str]]) -> Option<String> {
    sequences
        .iter()
        .find(|s| contains_sequence(words, s))
        .map(|s| s.join(" "))
}

fn analyse_phrases(a: &mut Analysis, ends_with_question: bool) {
    use SignalStrength::{Strong, Weak};
    let words = a.folded_words.clone();

    if let Some(seq) = first_sequence(&words, IMPACT_SEQUENCES) {
        a.signal(Intent::Impact, Strong, "impact-phrase", &seq);
    }
    if let Some(word) = first_word(&words, IMPACT_WORDS) {
        a.signal(Intent::Impact, Strong, "impact-word", word);
    }
    let where_used = words.iter().any(|w| w == "where")
        && words.iter().any(|w| w == "used" || w == "referenced");
    if where_used {
        a.signal(Intent::Impact, Strong, "where-used", "where … used");
    }
    let conditional = words
        .iter()
        .find(|w| w.chars().count() >= 6 && CONDITIONAL_SUFFIXES.iter().any(|s| w.ends_with(s)));
    if let Some(word) = conditional {
        a.signal(Intent::Impact, Strong, "conditional-change", word);
    }

    let causal = NOT_WHY_SEQUENCES
        .iter()
        .any(|s| contains_sequence(&words, s));
    let why_word = words
        .iter()
        .find(|w| WHY_WORDS.contains(&w.as_str()) && !(causal && w.as_str() == "neden"));
    if let Some(word) = why_word {
        a.signal(Intent::Why, Strong, "why-word", word);
    }
    if let Some(seq) = first_sequence(&words, WHY_SEQUENCES) {
        a.signal(Intent::Why, Strong, "why-phrase", &seq);
    }

    if let Some(word) = first_word(&words, ERROR_WORDS) {
        a.signal(Intent::ErrorTrace, Weak, "error-word", word);
    }
    if let Some(seq) = first_sequence(&words, ERROR_SEQUENCES) {
        a.signal(Intent::ErrorTrace, Weak, "error-word", &seq);
    }

    if let Some(word) = first_word(&words, ENDPOINT_WORDS) {
        a.signal(Intent::Endpoint, Weak, "endpoint-word", word);
    }
    if let Some(seq) = first_sequence(&words, ENDPOINT_SEQUENCES) {
        a.signal(Intent::Endpoint, Weak, "endpoint-word", &seq);
    }

    if let Some(word) = first_word(&words, QUESTION_WORDS) {
        a.signal(Intent::Behavior, Strong, "question-word", word);
    }
    if ends_with_question {
        a.signal(Intent::Behavior, Strong, "question-mark", "?");
    }
    if a.words.len() >= 3 {
        let evidence = a.words.join(" ");
        a.signal(Intent::Behavior, Strong, "natural-language", &evidence);
    }

    let decided = a.signals.iter().any(|s| tier(s).is_some());
    let short_keywords = (1..=2).contains(&a.words.len())
        && a.words
            .iter()
            .all(|w| is_identifier(w) || w.chars().all(|c| c.is_ascii_alphanumeric()));
    if !decided && short_keywords {
        let evidence = a.words.join(" ");
        a.signal(Intent::ExactSymbol, Weak, "short-keyword", &evidence);
    }
}

/// Precedence of signals: lower tiers win. `None` = informational only.
fn tier(signal: &Signal) -> Option<u8> {
    use Intent::*;
    use SignalStrength::{Strong, Weak};
    match (signal.intent, signal.strength) {
        (ErrorTrace, Strong) => Some(0),
        (Endpoint, Strong) => Some(1),
        (Impact, _) => Some(2),
        (Why, _) => Some(3),
        (ErrorTrace, Weak) => Some(4),
        (PathOrFile, _) => Some(5),
        (ExactSymbol, Strong) => Some(6),
        (Behavior, _) => Some(7),
        (ExactSymbol, Weak) => Some(8),
        (Endpoint, Weak) => None,
    }
}

fn decide(signals: &[Signal]) -> (Intent, Vec<Intent>) {
    let mut ranked: Vec<(u8, usize, Intent)> = signals
        .iter()
        .enumerate()
        .map(|(i, s)| (tier(s).unwrap_or(u8::MAX), i, s.intent))
        .collect();
    ranked.sort_unstable();
    let intent = ranked
        .first()
        .filter(|(t, _, _)| *t != u8::MAX)
        .map_or(Intent::Behavior, |(_, _, intent)| *intent);
    let mut secondary = Vec::new();
    for (_, _, other) in ranked {
        if other != intent && !secondary.contains(&other) {
            secondary.push(other);
        }
    }
    (intent, secondary)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clean_keeps_code_brackets() {
        assert_eq!(clean("(see"), "see");
        assert_eq!(clean("foo),"), "foo");
        assert_eq!(clean("/users/{id}"), "/users/{id}");
        assert_eq!(clean("foo()"), "foo()");
        assert_eq!(clean("(src/a.ts:42:7)"), "src/a.ts:42:7");
        assert_eq!(clean("((x))"), "x");
        assert_eq!(clean("end."), "end");
        assert_eq!(clean("TypeError:"), "TypeError");
        assert_eq!(clean("'quoted'"), "quoted");
        assert_eq!(clean(":::"), "");
    }

    #[test]
    fn tokenizer_keeps_quoted_spans() {
        let mut truncated = false;
        let tokens = tokenize("find `foo bar` and \"payment failed\" `x", &mut truncated);
        let texts: Vec<(&str, Option<Quote>)> =
            tokens.iter().map(|t| (t.text.as_str(), t.quote)).collect();
        assert_eq!(
            texts,
            [
                ("find", None),
                ("foo bar", Some(Quote::Backtick)),
                ("and", None),
                ("payment failed", Some(Quote::Double)),
                ("x", None),
            ]
        );
        assert!(!truncated);
    }

    #[test]
    fn shapes() {
        assert_eq!(identifier_rule("cancelSubscription"), Some("camel-case"));
        assert_eq!(identifier_rule("PaymentService"), Some("pascal-case"));
        assert_eq!(identifier_rule("retry_count"), Some("snake-case"));
        assert_eq!(identifier_rule("MAX_RETRIES"), Some("snake-case"));
        assert_eq!(identifier_rule("Payment"), None);
        assert_eq!(identifier_rule("payment"), None);
        assert!(is_qualified("Foo::bar"));
        assert!(is_qualified("a.b.c"));
        assert!(is_qualified("Invoice#total"));
        assert!(!is_qualified("e.g"));
        assert!(!is_qualified("example.com"));
        assert!(!is_qualified("service.ts"));
        assert!(!is_qualified("1.2.3"));
        assert!(is_error_code("E0308"));
        assert!(is_error_code("ERR_HTTP_HEADERS_SENT"));
        assert!(is_error_code("ORA-00942"));
        assert!(is_error_code("ECONNREFUSED"));
        assert!(!is_error_code("EXAMPLE"));
        assert!(is_route("/api/v1/users/{id}"));
        assert!(!is_route("//comment"));
        assert!(!is_route("/etc/app.conf"));
        assert_eq!(
            url_route("https://x.test/api/a?b=1").as_deref(),
            Some("/api/a")
        );
        assert_eq!(
            parse_location("src/app.ts:10:5"),
            Some(("src/app.ts".to_owned(), 10, Some(5)))
        );
        assert_eq!(parse_location("localhost:8080"), None);
        assert_eq!(path_shape("src/billing").as_deref(), Some("src/billing"));
        assert_eq!(path_shape("and/or"), None);
        assert_eq!(path_shape(".\\src\\a.ts").as_deref(), Some("src/a.ts"));
    }

    #[test]
    fn apostrophe_suffix() {
        assert_eq!(
            strip_apostrophe_suffix("PaymentService'i"),
            Some("PaymentService")
        );
        assert_eq!(strip_apostrophe_suffix("API’nin"), Some("API"));
        assert_eq!(strip_apostrophe_suffix("x'1"), None);
    }

    #[test]
    fn hostile_input_is_bounded() {
        let huge = "aB ".repeat(50_000);
        let p = plan(&huge, &Glossary::default());
        assert!(p.truncated);
        assert!(p.exact_terms.len() <= MAX_TERMS);
        let weird = "``\"\u{201C}((((]]]]::::##..//\\\\\0\u{7}";
        let p = plan(weird, &Glossary::default());
        assert_eq!(p.intent, Intent::Behavior);
        let p = plan("", &Glossary::default());
        assert!(p.is_empty());
    }
}
