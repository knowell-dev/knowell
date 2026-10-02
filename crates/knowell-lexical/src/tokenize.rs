//! Code-aware tokenizer shared by the index side and the query side.
//!
//! # Rules
//!
//! 1. **Runs.** The text is cut into runs of Unicode alphanumerics and `_`
//!    (plus combining marks, which stay attached to the preceding letter).
//!    Every other character (`/ . - : ( ) " ' ,` whitespace, emoji, ...)
//!    separates runs.
//! 2. **Normalisation.** Characters are lowercased (Unicode) and Turkish
//!    letters are folded to ASCII (`ç→c ğ→g ı→i İ→i ö→o ş→s ü→u`, and the
//!    circumflexed `â→a î→i û→u`). Turkish users type queries both with and
//!    without diacritics, while code and comments are often written without
//!    them (`odeme`, `iptal`); folding both sides makes the two spellings
//!    meet. The cost is a few harmless collisions (`sık` / `sik`), which a
//!    code search can afford. Combining marks (U+0300..=U+036F) are dropped so
//!    NFC and NFD spellings produce the same token.
//! 3. **Composite identifiers.** A run is split into *parts* on `_`,
//!    lower→Upper camel boundaries, acronym boundaries (`HTTPServer` →
//!    `http`, `server`) and letter↔digit boundaries. When a run has more than
//!    one part, the whole identifier is emitted first, then every part:
//!
//!    | input                 | tokens                                          |
//!    |-----------------------|-------------------------------------------------|
//!    | `cancel_subscription` | `cancel_subscription`, `cancel`, `subscription` |
//!    | `cancelSubscription`  | `cancelsubscription`, `cancel`, `subscription`  |
//!    | `HTTPServerError`     | `httpservererror`, `http`, `server`, `error`    |
//!    | `parseV2Config`       | `parsev2config`, `parse`, `v2`, `config`        |
//!
//!    Underscores are kept inside the whole-identifier token (so a snake_case
//!    query still hits the exact identifier) and stripped at its ends
//!    (`__init__` → `init`). A letter part directly followed by a digit part
//!    additionally emits a *glue* token (`v2`, `sha256`, `utf8`), because the
//!    digit alone is too short to survive the length filter and `v2` is a
//!    very common thing to search for.
//! 4. **Noise filters.** Tokens shorter than 2 characters, pure-digit tokens
//!    longer than 10 characters (ids, timestamps, hashes) and tokens longer
//!    than 64 characters are dropped. A run longer than 256 characters
//!    (base64 blobs, minified data) is skipped entirely instead of being
//!    split into random camel-case fragments. Lengths count characters of the
//!    normalised text.
//! 5. **Duplicates** inside one run are emitted once (`foo` → `foo`, not
//!    `foo`, `foo`).
//!
//! Offsets are byte offsets into the *original* text and positions count
//! emitted tokens. [`tokenize`] keeps every token; [`tokenize_query`] also
//! removes a small English + Turkish stopword list, which only makes sense
//! for natural-language questions and must not be applied to documents.

/// One normalised token with its location in the source text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Token {
    /// Normalised (lowercased, Turkish-folded) token text.
    pub text: String,
    /// Byte offset in the original text where the token's source begins.
    pub offset_from: usize,
    /// Byte offset in the original text just past the token's source.
    pub offset_to: usize,
    /// Zero-based ordinal of the token among the emitted tokens.
    pub position: usize,
}

/// Tokens shorter than this many characters are dropped.
const MIN_TOKEN_CHARS: usize = 2;
/// Tokens longer than this many characters are dropped.
const MAX_TOKEN_CHARS: usize = 64;
/// Runs longer than this many characters are skipped entirely.
const MAX_RUN_CHARS: usize = 256;
/// Pure-digit tokens longer than this many characters are dropped.
const MAX_NUMERIC_CHARS: usize = 10;

/// Stopwords removed by [`tokenize_query`], already normalised (folded).
const STOPWORDS: &[&str] = &[
    "the", "an", "of", "to", "in", "is", "are", "where", "how", "do", "does", "we", "what",
    "which", "and", "or", "for", "on", "it", "as", "be", "ve", "bir", "bu", "nerede", "nasil",
    "ne", "hangi", "mi", "mu", "icin", "ile",
];

#[derive(Clone, Copy, PartialEq, Eq)]
enum Class {
    Lower,
    Upper,
    Digit,
    Underscore,
}

/// One non-mark character of a run with its source and normalised ranges.
struct Unit {
    class: Class,
    from: usize,
    to: usize,
    n_from: usize,
    n_to: usize,
}

/// A contiguous group of units forming one identifier part (unit indices).
struct Part {
    start: usize,
    end: usize,
}

struct Emitter {
    out: Vec<Token>,
    seen: Vec<String>,
}

impl Emitter {
    fn emit(&mut self, text: &str, from: usize, to: usize, position: &mut usize) {
        let chars = text.chars().count();
        if !(MIN_TOKEN_CHARS..=MAX_TOKEN_CHARS).contains(&chars) {
            return;
        }
        if chars > MAX_NUMERIC_CHARS && text.chars().all(char::is_numeric) {
            return;
        }
        if self.seen.iter().any(|s| s == text) {
            return;
        }
        self.seen.push(text.to_owned());
        self.out.push(Token {
            text: text.to_owned(),
            offset_from: from,
            offset_to: to,
            position: *position,
        });
        *position += 1;
    }
}

fn is_mark(c: char) -> bool {
    ('\u{300}'..='\u{36f}').contains(&c)
}

fn is_word_char(c: char) -> bool {
    c == '_' || c.is_alphanumeric() || is_mark(c)
}

fn classify(c: char) -> Class {
    if c == '_' {
        Class::Underscore
    } else if c.is_numeric() {
        Class::Digit
    } else if c.is_uppercase() {
        Class::Upper
    } else {
        Class::Lower
    }
}

/// Lowercases and Turkish-folds one character into `out`.
///
/// The Turkish letters are mapped explicitly before `to_lowercase` because
/// `İ` would otherwise lowercase to `i` plus a combining dot.
fn push_normalized(c: char, out: &mut String) {
    let folded = match c {
        'ç' | 'Ç' => 'c',
        'ğ' | 'Ğ' => 'g',
        'ı' | 'İ' | 'î' | 'Î' => 'i',
        'ö' | 'Ö' => 'o',
        'ş' | 'Ş' => 's',
        'ü' | 'Ü' | 'û' | 'Û' => 'u',
        'â' | 'Â' => 'a',
        other => {
            out.extend(other.to_lowercase());
            return;
        }
    };
    out.push(folded);
}

/// Splits `units` into parts at `_`, camel, acronym and letter/digit boundaries.
fn split_parts(units: &[Unit]) -> Vec<Part> {
    let mut parts = Vec::new();
    let n = units.len();
    let mut i = 0;
    while i < n {
        if units.get(i).map(|u| u.class) == Some(Class::Underscore) {
            i += 1;
            continue;
        }
        let mut j = i;
        while units.get(j).is_some_and(|u| u.class != Class::Underscore) {
            j += 1;
        }
        let mut start = i;
        for k in (i + 1)..j {
            let prev = units.get(k - 1).map(|u| u.class);
            let cur = units.get(k).map(|u| u.class);
            let next = units.get(k + 1).map(|u| u.class);
            let boundary = match (prev, cur) {
                (Some(Class::Lower), Some(Class::Upper)) => true,
                (Some(Class::Upper), Some(Class::Upper)) => next == Some(Class::Lower),
                (Some(Class::Digit), Some(Class::Digit)) => false,
                (Some(Class::Digit), _) | (_, Some(Class::Digit)) => true,
                _ => false,
            };
            if boundary {
                parts.push(Part { start, end: k });
                start = k;
            }
        }
        parts.push(Part { start, end: j });
        i = j;
    }
    parts
}

fn emit_run(text: &str, from: usize, to: usize, position: &mut usize, out: &mut Vec<Token>) {
    let Some(run) = text.get(from..to) else {
        return;
    };
    let mut units: Vec<Unit> = Vec::new();
    let mut norm = String::new();
    for (off, c) in run.char_indices() {
        let abs = from + off;
        let end = abs + c.len_utf8();
        if is_mark(c) {
            if let Some(last) = units.last_mut() {
                last.to = end;
            }
            continue;
        }
        if units.len() >= MAX_RUN_CHARS {
            return;
        }
        let n_from = norm.len();
        push_normalized(c, &mut norm);
        units.push(Unit {
            class: classify(c),
            from: abs,
            to: end,
            n_from,
            n_to: norm.len(),
        });
    }

    let parts = split_parts(&units);
    let mut em = Emitter {
        out: std::mem::take(out),
        seen: Vec::new(),
    };

    // Helper closures cannot borrow `em` twice, so ranges are resolved here.
    let unit_range = |start: usize, end: usize| -> Option<(&str, usize, usize)> {
        let first = units.get(start)?;
        let last = units.get(end.checked_sub(1)?)?;
        let s = norm.get(first.n_from..last.n_to)?;
        Some((s, first.from, last.to))
    };

    if parts.len() > 1
        && let (Some(first), Some(last)) = (parts.first(), parts.last())
        && let Some((s, f, t)) = unit_range(first.start, last.end)
    {
        em.emit(s, f, t, position);
    }
    for (idx, part) in parts.iter().enumerate() {
        if let Some((s, f, t)) = unit_range(part.start, part.end) {
            em.emit(s, f, t, position);
        }
        // Glue: a letter part immediately followed by a digit part (`v` + `2`).
        let is_digit = units.get(part.start).map(|u| u.class) == Some(Class::Digit);
        if is_digit
            && idx > 0
            && let Some(prev) = parts.get(idx - 1)
            && units.get(prev.start).map(|u| u.class) != Some(Class::Digit)
            && prev.end == part.start
            && let Some((s, f, t)) = unit_range(prev.start, part.end)
        {
            em.emit(s, f, t, position);
        }
    }
    *out = em.out;
}

/// Tokenizes `text` for indexing (all tokens kept). See the module docs.
///
/// Never panics; empty or separator-only input yields an empty vector.
#[must_use]
pub fn tokenize(text: &str) -> Vec<Token> {
    let mut out = Vec::new();
    let mut position = 0usize;
    let mut run_start: Option<usize> = None;
    for (i, c) in text.char_indices() {
        if is_word_char(c) {
            if run_start.is_none() {
                run_start = Some(i);
            }
        } else if let Some(s) = run_start.take() {
            emit_run(text, s, i, &mut position, &mut out);
        }
    }
    if let Some(s) = run_start {
        emit_run(text, s, text.len(), &mut position, &mut out);
    }
    out
}

/// Tokenizes a search query: like [`tokenize`] but without stopwords.
///
/// If the query consists of nothing but stopwords (for example `"how"`), the
/// unfiltered tokens are returned so that the user still gets results for
/// what they typed.
#[must_use]
pub fn tokenize_query(text: &str) -> Vec<Token> {
    let all = tokenize(text);
    let filtered: Vec<Token> = all
        .iter()
        .filter(|t| !STOPWORDS.contains(&t.text.as_str()))
        .cloned()
        .collect();
    if filtered.is_empty() { all } else { filtered }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing)]
mod tests {
    use super::*;

    fn texts(s: &str) -> Vec<String> {
        tokenize(s).into_iter().map(|t| t.text).collect()
    }

    #[test]
    fn empty_and_separator_only() {
        assert!(tokenize("").is_empty());
        assert!(tokenize(" \t\r\n ./-:()\"',").is_empty());
        assert!(tokenize("___").is_empty());
    }

    #[test]
    fn snake_case() {
        assert_eq!(
            texts("cancel_subscription"),
            ["cancel_subscription", "cancel", "subscription"]
        );
    }

    #[test]
    fn camel_case() {
        assert_eq!(
            texts("cancelSubscription"),
            ["cancelsubscription", "cancel", "subscription"]
        );
    }

    #[test]
    fn acronym() {
        assert_eq!(
            texts("HTTPServerError"),
            ["httpservererror", "http", "server", "error"]
        );
    }

    #[test]
    fn digits_and_glue() {
        assert_eq!(
            texts("parseV2Config"),
            ["parsev2config", "parse", "v2", "config"]
        );
        assert_eq!(texts("sha256"), ["sha256", "sha", "256"]);
    }

    #[test]
    fn leading_trailing_underscores_trimmed() {
        assert_eq!(texts("__init__"), ["init"]);
        assert_eq!(texts("_my_var_"), ["my_var", "my", "var"]);
    }

    #[test]
    fn plain_word_is_single_token() {
        assert_eq!(texts("Subscription"), ["subscription"]);
        assert_eq!(texts("foo foo"), ["foo", "foo"]);
    }

    #[test]
    fn path_separators() {
        assert_eq!(
            texts("billing-api/src/subscription.service.ts"),
            ["billing", "api", "src", "subscription", "service", "ts"]
        );
    }

    #[test]
    fn short_tokens_dropped() {
        assert_eq!(texts("a b c ab"), ["ab"]);
        assert_eq!(texts("x_y"), ["x_y"]);
    }

    #[test]
    fn long_digits_dropped() {
        assert!(texts("12345678901").is_empty());
        assert_eq!(texts("1234567890"), ["1234567890"]);
        assert_eq!(texts("2024"), ["2024"]);
    }

    #[test]
    fn long_tokens_capped() {
        let ok = "a".repeat(64);
        let bad = "a".repeat(65);
        assert_eq!(texts(&ok), std::slice::from_ref(&ok));
        assert!(texts(&bad).is_empty());
        let blob = "aB".repeat(200);
        assert!(texts(&blob).is_empty());
    }

    #[test]
    fn turkish_folding() {
        assert_eq!(texts("Ödeme"), ["odeme"]);
        assert_eq!(texts("İPTAL ışık çğüşö"), ["iptal", "isik", "cgus\u{6f}"]);
        assert_eq!(texts("Âdet Îman Ûstad"), ["adet", "iman", "ustad"]);
    }

    #[test]
    fn nfd_equals_nfc() {
        assert_eq!(texts("o\u{308}deme"), texts("ödeme"));
        assert_eq!(texts("e\u{301}tude"), ["etude"]);
    }

    #[test]
    fn unicode_letters_kept() {
        assert_eq!(texts("日本語 テスト"), ["日本語", "テスト"]);
        assert_eq!(texts("Straße"), ["straße"]);
    }

    #[test]
    fn emoji_separates() {
        assert_eq!(texts("ship🚀it"), ["ship", "it"]);
        assert!(texts("🚀🔥").is_empty());
    }

    #[test]
    fn crlf() {
        assert_eq!(texts("foo\r\nbar\r\n"), ["foo", "bar"]);
    }

    #[test]
    fn very_long_input_does_not_blow_up() {
        let text = "word ".repeat(200_000);
        assert_eq!(tokenize(&text).len(), 200_000);
        let one_run = "a".repeat(2_000_000);
        assert!(tokenize(&one_run).is_empty());
    }

    #[test]
    fn offsets_and_positions() {
        let src = "  fooBar baz";
        let toks = tokenize(src);
        let got: Vec<_> = toks
            .iter()
            .map(|t| {
                (
                    t.text.as_str(),
                    &src[t.offset_from..t.offset_to],
                    t.position,
                )
            })
            .collect();
        assert_eq!(
            got,
            [
                ("foobar", "fooBar", 0),
                ("foo", "foo", 1),
                ("bar", "Bar", 2),
                ("baz", "baz", 3)
            ]
        );
    }

    #[test]
    fn offsets_valid_char_boundaries_for_unicode() {
        let src = "Ödeme_şablonu 🚀 İptalEt";
        for t in tokenize(src) {
            assert!(src.get(t.offset_from..t.offset_to).is_some());
        }
    }

    #[test]
    fn query_drops_stopwords() {
        let q: Vec<String> = tokenize_query("where is the cancelSubscription")
            .into_iter()
            .map(|t| t.text)
            .collect();
        assert_eq!(q, ["cancelsubscription", "cancel", "subscription"]);
        let q: Vec<String> = tokenize_query("Ödeme nerede nasıl yapılır")
            .into_iter()
            .map(|t| t.text)
            .collect();
        assert_eq!(q, ["odeme", "yapilir"]);
    }

    #[test]
    fn query_all_stopwords_falls_back() {
        let q: Vec<String> = tokenize_query("how").into_iter().map(|t| t.text).collect();
        assert_eq!(q, ["how"]);
        assert!(tokenize_query("").is_empty());
    }

    #[test]
    fn documents_keep_stopwords() {
        assert_eq!(texts("where the"), ["where", "the"]);
    }
}
