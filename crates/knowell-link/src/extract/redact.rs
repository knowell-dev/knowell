//! Secret redaction that keeps line numbers stable.

/// Replaces every secret-shaped span found by `knowell_secrets::scan` with
/// `[REDACTED:<kind>]` followed by as many newlines as the span contained,
/// so line ranges computed on the result are valid for the original file.
pub(crate) fn redact_keep_lines(text: &str) -> String {
    let findings = knowell_secrets::scan(text);
    if findings.is_empty() {
        return text.to_owned();
    }
    let mut out = String::with_capacity(text.len());
    let mut cursor = 0usize;
    for finding in &findings {
        if finding.start < cursor {
            continue;
        }
        if let Some(kept) = text.get(cursor..finding.start) {
            out.push_str(kept);
        }
        out.push_str("[REDACTED:");
        out.push_str(finding.kind.as_str());
        out.push(']');
        let newlines = text
            .get(finding.start..finding.end)
            .map_or(0, |span| span.matches('\n').count());
        for _ in 0..newlines {
            out.push('\n');
        }
        cursor = finding.end;
    }
    if let Some(rest) = text.get(cursor..) {
        out.push_str(rest);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keeps_lines_and_removes_values() {
        let key = format!("AKIA{}", "FAKE".repeat(4));
        let text = format!("a\nconst k = \"{key}\";\nb\n");
        let out = redact_keep_lines(&text);
        assert!(!out.contains(&key));
        assert_eq!(out.lines().count(), text.lines().count());
        let pem = format!(
            "x\n-----BEGIN {k} PRIVATE KEY-----\n{body}\n-----END {k} PRIVATE KEY-----\ny\n",
            k = "RSA",
            body = "FAKEFAKE".repeat(8)
        );
        let out = redact_keep_lines(&pem);
        assert_eq!(out.lines().count(), pem.lines().count());
        assert_eq!(out.lines().last(), Some("y"));
        assert_eq!(redact_keep_lines("plain"), "plain");
    }
}
