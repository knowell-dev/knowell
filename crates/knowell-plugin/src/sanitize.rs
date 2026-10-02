//! Making untrusted plugin text safe to put in logs and error messages.

/// Replacement for characters that could forge log lines or reorder text.
const REPLACEMENT: char = '\u{FFFD}';

/// Returns `text` as a single line of at most `max_bytes` bytes (plus a
/// trailing `…` when cut): line breaks and tabs become spaces, other control
/// characters and bidirectional/zero-width format characters become U+FFFD,
/// so a plugin cannot forge extra log lines or visually reorder text.
pub(crate) fn sanitize(text: &str, max_bytes: usize) -> String {
    let mut out = String::with_capacity(text.len().min(max_bytes).saturating_add(3));
    for ch in text.chars() {
        let ch = match ch {
            '\n' | '\r' | '\t' => ' ',
            c if c.is_control() || is_format_control(c) => REPLACEMENT,
            c => c,
        };
        if out.len().saturating_add(ch.len_utf8()) > max_bytes {
            out.push('…');
            break;
        }
        out.push(ch);
    }
    out
}

/// Zero-width and bidirectional formatting characters (Unicode `Cf` subset
/// used in "trojan source" style spoofing).
fn is_format_control(c: char) -> bool {
    matches!(
        c,
        '\u{200B}'..='\u{200F}'
            | '\u{202A}'..='\u{202E}'
            | '\u{2060}'..='\u{2064}'
            | '\u{2066}'..='\u{2069}'
            | '\u{FEFF}'
    )
}

/// The last `max_bytes` bytes of `bytes` as sanitised text (lossy UTF-8),
/// prefixed with `…` when cut. `None` when there is nothing but whitespace.
pub(crate) fn tail(bytes: &[u8], max_bytes: usize) -> Option<String> {
    let text = String::from_utf8_lossy(bytes);
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return None;
    }
    let mut start = trimmed.len().saturating_sub(max_bytes);
    while !trimmed.is_char_boundary(start) {
        start = start.saturating_add(1);
    }
    let cut = trimmed.get(start..).unwrap_or_default();
    let body = sanitize(cut, max_bytes);
    Some(if start > 0 {
        format!("…{body}")
    } else {
        body
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keeps_plain_text() {
        assert_eq!(sanitize("GET /users/{id}", 64), "GET /users/{id}");
    }

    #[test]
    fn flattens_lines_and_replaces_controls() {
        assert_eq!(sanitize("a\nb\tc\r", 64), "a b c ");
        assert_eq!(sanitize("x\u{1b}[31my\0", 64), "x\u{FFFD}[31my\u{FFFD}");
        assert_eq!(sanitize("evil\u{202E}txt", 64), "evil\u{FFFD}txt");
        assert_eq!(sanitize("zero\u{200B}width", 64), "zero\u{FFFD}width");
    }

    #[test]
    fn truncates_on_char_boundaries() {
        assert_eq!(sanitize("abcdef", 3), "abc…");
        // 'é' is two bytes and must not be split.
        assert_eq!(sanitize("aé", 2), "a…");
        assert_eq!(sanitize("", 0), "");
        assert_eq!(sanitize("a", 0), "…");
    }

    #[test]
    fn tail_keeps_the_end() {
        assert_eq!(tail(b"  \n ", 10), None);
        assert_eq!(tail(b"short", 10).as_deref(), Some("short"));
        assert_eq!(tail(b"0123456789abc", 3).as_deref(), Some("…abc"));
        // Invalid UTF-8 and split multibyte characters are tolerated.
        assert_eq!(tail(&[0xff, b'o', b'k'], 10).as_deref(), Some("\u{FFFD}ok"));
        assert_eq!(tail("ééé".as_bytes(), 3).as_deref(), Some("…é"));
    }
}
