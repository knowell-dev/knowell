//! Resolution of [`SecretRef`]s into secret values at the moment of use.

use std::fs;

use knowell_core::SecretRef;
use secrecy::SecretString;

use crate::error::SecretsError;

/// Resolves a secret reference.
///
/// - `env:NAME` reads the process environment.
/// - `file:PATH` reads the file as UTF-8 and removes one trailing `\n` or
///   `\r\n`.
///
/// An empty value is rejected. Errors mention the reference and the failure
/// kind only, never the value or any file content.
pub fn resolve(reference: &SecretRef) -> Result<SecretString, SecretsError> {
    let described = reference.describe();
    let mut value = match reference {
        SecretRef::Env(name) => std::env::var(name).map_err(|e| match e {
            std::env::VarError::NotPresent => SecretsError::EnvNotSet {
                reference: described.clone(),
            },
            std::env::VarError::NotUnicode(_) => SecretsError::EnvNotUnicode {
                reference: described.clone(),
            },
        })?,
        SecretRef::File(path) => {
            let bytes = fs::read(path).map_err(|e| SecretsError::FileUnreadable {
                reference: described.clone(),
                kind: format!("{:?}", e.kind()),
            })?;
            // `FromUtf8Error` holds the bytes; discard it without formatting.
            String::from_utf8(bytes).map_err(|_| SecretsError::FileNotUtf8 {
                reference: described.clone(),
            })?
        }
    };
    if matches!(reference, SecretRef::File(_)) && value.ends_with('\n') {
        value.pop();
        if value.ends_with('\r') {
            value.pop();
        }
    }
    if value.is_empty() {
        return Err(SecretsError::Empty {
            reference: described,
        });
    }
    Ok(SecretString::from(value))
}

#[cfg(test)]
mod tests {
    use std::io::Write;

    use secrecy::ExposeSecret;

    use super::*;

    fn file_ref(path: &std::path::Path) -> SecretRef {
        SecretRef::File(path.to_path_buf())
    }

    fn write(dir: &tempfile::TempDir, name: &str, bytes: &[u8]) -> std::path::PathBuf {
        let path = dir.path().join(name);
        let mut f = std::fs::File::create(&path).unwrap();
        f.write_all(bytes).unwrap();
        path
    }

    #[test]
    fn file_trims_one_trailing_newline() {
        let dir = tempfile::tempdir().unwrap();
        let canary = format!("KNOWELL_CANARY_{}", "11aa");
        for (name, suffix, expect) in [
            ("a", "\n", canary.clone()),
            ("b", "\r\n", canary.clone()),
            ("c", "", canary.clone()),
            ("d", "\n\n", format!("{canary}\n")),
        ] {
            let p = write(&dir, name, format!("{canary}{suffix}").as_bytes());
            let v = resolve(&file_ref(&p)).unwrap();
            assert_eq!(v.expose_secret(), expect, "{name}");
        }
    }

    #[test]
    fn empty_file_is_rejected() {
        let dir = tempfile::tempdir().unwrap();
        for (name, body) in [("e1", ""), ("e2", "\n"), ("e3", "\r\n")] {
            let p = write(&dir, name, body.as_bytes());
            assert!(matches!(
                resolve(&file_ref(&p)),
                Err(SecretsError::Empty { .. })
            ));
        }
    }

    #[test]
    fn missing_file_reports_kind_only() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("nope");
        let err = resolve(&file_ref(&p)).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("NotFound"), "{msg}");
        assert!(msg.contains("file:"), "{msg}");
    }

    #[test]
    fn non_utf8_file_does_not_leak() {
        let dir = tempfile::tempdir().unwrap();
        let mut body = b"KNOWELL_CANARY_9z".to_vec();
        body.push(0xff);
        let p = write(&dir, "bin", &body);
        let err = resolve(&file_ref(&p)).unwrap_err();
        assert!(matches!(err, SecretsError::FileNotUtf8 { .. }));
        assert!(!err.to_string().contains("CANARY"));
        assert!(!format!("{err:?}").contains("CANARY"));
    }

    #[test]
    fn unset_env_is_reported_by_name() {
        let r = SecretRef::Env("KNOWELL_TEST_SURELY_UNSET_VARIABLE_42".into());
        let err = resolve(&r).unwrap_err();
        assert!(matches!(err, SecretsError::EnvNotSet { .. }));
        assert!(err.to_string().contains("env:KNOWELL_TEST_SURELY_UNSET"));
    }

    #[test]
    fn debug_of_resolved_secret_is_redacted() {
        let dir = tempfile::tempdir().unwrap();
        let p = write(&dir, "k", b"KNOWELL_CANARY_dbg\n");
        let v = resolve(&file_ref(&p)).unwrap();
        assert!(!format!("{v:?}").contains("CANARY"));
    }
}
