//! Secret guard: free text is scanned before it may enter memory.

use knowell_secrets::scan;

use crate::error::KnowledgeError;

/// Rejects `text` if it contains a secret.
///
/// The error names the field, the finding kind and the line; it never
/// contains any part of the secret.
pub fn check_no_secrets(field: &'static str, text: &str) -> Result<(), KnowledgeError> {
    match scan(text).first() {
        None => Ok(()),
        Some(finding) => Err(KnowledgeError::SecretDetected {
            field,
            kind: finding.kind.as_str(),
            line: finding.line,
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fake_github_token() -> String {
        format!("ghp_{}", "FAKE".repeat(9))
    }

    #[test]
    fn clean_text_passes() {
        assert_eq!(
            check_no_secrets("body", "use the idempotency key header"),
            Ok(())
        );
    }

    #[test]
    fn secret_is_rejected_without_echoing_it() {
        let token = fake_github_token();
        let text = format!("first line\nsecond: {token}\n");
        let err = check_no_secrets("body", &text).unwrap_err();
        assert_eq!(
            err,
            KnowledgeError::SecretDetected {
                field: "body",
                kind: "github_token",
                line: 2
            }
        );
        let shown = format!("{err} / {err:?}");
        assert!(!shown.contains(&token));
        assert!(!shown.contains("FAKEFAKE"));
    }
}
