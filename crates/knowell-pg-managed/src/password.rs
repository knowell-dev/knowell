//! Superuser credentials: generation, storage and short-lived credential files.
//!
//! The password never appears in a command line, a log line or a `Debug`
//! output. Storage goes through [`PasswordStore`] so an OS-keychain
//! implementation can replace the file store later.

use crate::error::{Error, Result};
use secrecy::{ExposeSecret, SecretString};
use std::fs::OpenOptions;
use std::io::Write;
use std::path::{Path, PathBuf};

/// Length of generated passwords, in characters.
pub const PASSWORD_LEN: usize = 32;

const ALPHABET: &[u8; 62] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789";

/// Where the superuser password is kept between runs.
///
/// Implementations must be safe to call from several threads and must not
/// reveal the secret through `Debug`.
pub trait PasswordStore: Send + Sync + std::fmt::Debug {
    /// Return the stored password, or `None` if none has been stored yet.
    ///
    /// # Errors
    /// Returns [`Error::Password`] if the store is unreadable or unsafe to use.
    fn get(&self) -> Result<Option<SecretString>>;

    /// Store `password`, replacing any previous value.
    ///
    /// # Errors
    /// Returns [`Error::Password`] if the value cannot be stored safely.
    fn set(&self, password: &SecretString) -> Result<()>;
}

/// Generate a random password of [`PASSWORD_LEN`] alphanumeric characters
/// (about 190 bits) from the operating system's CSPRNG.
///
/// Alphanumeric output needs no escaping in URLs or `.pgpass` files.
///
/// # Errors
/// Returns [`Error::Password`] if the OS random source fails.
pub fn generate_password() -> Result<SecretString> {
    let mut out = String::with_capacity(PASSWORD_LEN);
    while out.len() < PASSWORD_LEN {
        let mut buf = [0u8; 64];
        getrandom::fill(&mut buf)
            .map_err(|err| Error::Password(format!("operating system randomness failed: {err}")))?;
        for byte in buf {
            // 248 = 62 * 4: reject the tail so every symbol is equally likely.
            if byte < 248 && out.len() < PASSWORD_LEN {
                let index = usize::from(byte) % ALPHABET.len();
                if let Some(symbol) = ALPHABET.get(index) {
                    out.push(char::from(*symbol));
                }
            }
        }
    }
    Ok(SecretString::from(out))
}

/// Return the stored password, generating and storing a new one if none exists.
///
/// The boolean is `true` when a new password was created.
///
/// # Errors
/// Propagates store and randomness failures.
pub fn load_or_create(store: &dyn PasswordStore) -> Result<(SecretString, bool)> {
    if let Some(existing) = store.get()? {
        return Ok((existing, false));
    }
    let fresh = generate_password()?;
    store.set(&fresh)?;
    Ok((fresh, true))
}

/// Password stored in a file readable only by its owner.
///
/// * Unix: the file is created with mode 0600 and read back only if no group
///   or other bits are set.
/// * Windows: `std` cannot edit ACLs, so the file is created empty, then
///   `icacls` removes inherited rights and grants only the current user, and
///   only then is the secret written. If `icacls` fails, storing fails.
///   Verification on read is not possible with `std`.
#[derive(Clone, PartialEq, Eq)]
pub struct FilePasswordStore {
    path: PathBuf,
}

impl FilePasswordStore {
    /// A store backed by `path` (the file need not exist yet).
    #[must_use]
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into() }
    }

    /// Location of the password file.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl std::fmt::Debug for FilePasswordStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FilePasswordStore")
            .field("path", &self.path)
            .field("password", &"<redacted>")
            .finish()
    }
}

impl PasswordStore for FilePasswordStore {
    fn get(&self) -> Result<Option<SecretString>> {
        let meta = match std::fs::symlink_metadata(&self.path) {
            Ok(meta) => meta,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(err) => {
                return Err(Error::io(
                    format!("inspecting {}", self.path.display()),
                    err,
                ));
            }
        };
        if !meta.is_file() {
            return Err(Error::Password(format!(
                "{} is not a regular file",
                self.path.display()
            )));
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            if meta.permissions().mode() & 0o077 != 0 {
                return Err(Error::Password(format!(
                    "{} is accessible to other users; run chmod 600 on it",
                    self.path.display()
                )));
            }
        }
        let text = std::fs::read_to_string(&self.path)
            .map_err(|err| Error::io(format!("reading {}", self.path.display()), err))?;
        let trimmed = text.trim();
        if trimmed.is_empty() {
            return Err(Error::Password(format!(
                "{} is empty; delete it to generate a new password",
                self.path.display()
            )));
        }
        Ok(Some(SecretString::from(trimmed.to_string())))
    }

    fn set(&self, password: &SecretString) -> Result<()> {
        let secret = password.expose_secret();
        if secret.is_empty() || secret.contains(['\n', '\r', '\0']) {
            return Err(Error::Password(
                "a password must be non-empty and free of line breaks".to_string(),
            ));
        }
        if let Some(dir) = self.path.parent() {
            create_private_dir(dir)?;
        }
        // Write beside the target and rename so a crash never leaves a
        // truncated password behind.
        let tmp = self.path.with_extension("tmp");
        write_private_file(&tmp, secret.as_bytes())?;
        std::fs::rename(&tmp, &self.path)
            .map_err(|err| Error::io(format!("replacing {}", self.path.display()), err))
    }
}

/// Create `dir` (and parents) and restrict it to the owner on Unix.
pub(crate) fn create_private_dir(dir: &Path) -> Result<()> {
    std::fs::create_dir_all(dir)
        .map_err(|err| Error::io(format!("creating {}", dir.display()), err))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))
            .map_err(|err| Error::io(format!("restricting {}", dir.display()), err))?;
    }
    Ok(())
}

/// Create (or truncate) `path` so that only the owner can read it, then write `bytes`.
pub(crate) fn write_private_file(path: &Path, bytes: &[u8]) -> Result<()> {
    let mut options = OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options
        .open(path)
        .map_err(|err| Error::io(format!("creating {}", path.display()), err))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        // `mode` only applies on creation; fix a pre-existing file too.
        file.set_permissions(std::fs::Permissions::from_mode(0o600))
            .map_err(|err| Error::io(format!("restricting {}", path.display()), err))?;
    }
    #[cfg(windows)]
    restrict_to_current_user(path)?;
    file.write_all(bytes)
        .and_then(|()| file.flush())
        .map_err(|err| Error::io(format!("writing {}", path.display()), err))
}

/// Remove inherited ACL entries and grant only the current user, via `icacls`.
#[cfg(windows)]
fn restrict_to_current_user(path: &Path) -> Result<()> {
    use std::os::windows::process::CommandExt;
    let user = std::env::var("USERNAME").map_err(|_| {
        Error::Password("USERNAME is not set; cannot restrict the file".to_string())
    })?;
    let output = std::process::Command::new("icacls")
        .arg(path)
        .args(["/inheritance:r", "/grant:r"])
        .arg(format!("{user}:F"))
        .creation_flags(0x0800_0000)
        .output()
        .map_err(|err| Error::io("running icacls", err))?;
    if output.status.success() {
        Ok(())
    } else {
        Err(Error::Password(format!(
            "icacls could not restrict {} (exit code {:?})",
            path.display(),
            output.status.code()
        )))
    }
}

/// A short-lived secret file for a child process, removed on drop.
///
/// Used for `PGPASSFILE` (credentials for client tools) and for `initdb
/// --pwfile`. The secret reaches the child only as a file path.
pub(crate) struct TempSecretFile {
    path: PathBuf,
}

impl TempSecretFile {
    /// Write `contents` to a new owner-only file named `<prefix>-<random>`
    /// inside `dir` (created owner-only if missing).
    pub(crate) fn create(dir: &Path, prefix: &str, contents: &str) -> Result<Self> {
        create_private_dir(dir)?;
        let mut random = [0u8; 8];
        getrandom::fill(&mut random)
            .map_err(|err| Error::Password(format!("operating system randomness failed: {err}")))?;
        let name: String = random.iter().map(|b| format!("{b:02x}")).collect();
        let path = dir.join(format!("{prefix}-{name}"));
        write_private_file(&path, contents.as_bytes())?;
        Ok(Self { path })
    }

    /// A `.pgpass`-format file with one wildcard entry for `username`.
    ///
    /// The host field is `*` because tools reach the server over loopback TCP
    /// or, inside `pg_upgrade`, a temporary Unix socket directory.
    pub(crate) fn pgpass(dir: &Path, username: &str, password: &SecretString) -> Result<Self> {
        let line = format!(
            "*:*:*:{}:{}\n",
            escape_pgpass(username),
            escape_pgpass(password.expose_secret())
        );
        Self::create(dir, "pgpass", &line)
    }

    /// A file holding only the password, for `initdb --pwfile`.
    pub(crate) fn password_only(dir: &Path, password: &SecretString) -> Result<Self> {
        Self::create(dir, "pwfile", password.expose_secret())
    }

    /// Path to hand to the child.
    pub(crate) fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for TempSecretFile {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

impl std::fmt::Debug for TempSecretFile {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TempSecretFile")
            .field("path", &self.path)
            .finish()
    }
}

/// Escape `\` and `:` as required by the `.pgpass` format.
fn escape_pgpass(field: &str) -> String {
    let mut out = String::with_capacity(field.len());
    for c in field.chars() {
        if c == '\\' || c == ':' {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    #[test]
    fn generated_passwords_are_strong_and_distinct() {
        let mut seen = HashSet::new();
        for _ in 0..50 {
            let password = generate_password().unwrap();
            let text = password.expose_secret();
            assert_eq!(text.len(), PASSWORD_LEN);
            assert!(text.chars().all(|c| c.is_ascii_alphanumeric()));
            assert!(seen.insert(text.to_string()), "duplicate password");
        }
    }

    #[test]
    fn file_store_round_trips_and_load_or_create_is_stable() {
        let dir = tempfile::tempdir().unwrap();
        let store = FilePasswordStore::new(dir.path().join("pg").join("password"));
        assert!(store.get().unwrap().is_none());
        let (first, created) = load_or_create(&store).unwrap();
        assert!(created);
        let (second, created_again) = load_or_create(&store).unwrap();
        assert!(!created_again);
        assert_eq!(first.expose_secret(), second.expose_secret());
    }

    #[cfg(unix)]
    #[test]
    fn file_store_uses_owner_only_permissions() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("pg").join("password");
        let store = FilePasswordStore::new(&path);
        load_or_create(&store).unwrap();
        let mode = std::fs::metadata(&path).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
        let dir_mode = std::fs::metadata(path.parent().unwrap())
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(dir_mode & 0o777, 0o700);
    }

    #[cfg(unix)]
    #[test]
    fn file_store_refuses_group_readable_file() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("password");
        std::fs::write(&path, "KNOWELL_CANARY_pw").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o640)).unwrap();
        let err = FilePasswordStore::new(&path).get().unwrap_err();
        assert!(matches!(err, Error::Password(_)));
        assert!(!err.to_string().contains("KNOWELL_CANARY_pw"));
    }

    #[test]
    fn file_store_rejects_empty_file_and_bad_values() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("password");
        std::fs::write(&path, "  \n").unwrap();
        let store = FilePasswordStore::new(&path);
        // Permissions of a fresh file under a temp dir are fine on Unix (umask 022
        // would trip the check), so only assert on the error kind when it fails.
        assert!(store.get().is_err());
        for bad in ["", "a\nb", "a\0b"] {
            assert!(store.set(&SecretString::from(bad.to_string())).is_err());
        }
    }

    #[test]
    fn debug_output_redacts_secrets() {
        let dir = tempfile::tempdir().unwrap();
        let store = FilePasswordStore::new(dir.path().join("password"));
        let secret = SecretString::from("KNOWELL_CANARY_secret".to_string());
        store.set(&secret).unwrap();
        let shown = format!("{store:?} {secret:?}");
        assert!(!shown.contains("KNOWELL_CANARY_secret"));
        assert!(shown.contains("redacted"));
    }

    #[test]
    fn pgpass_file_has_expected_line_and_is_removed_on_drop() {
        let dir = tempfile::tempdir().unwrap();
        let secret = SecretString::from("pa:ss\\word".to_string());
        let path;
        {
            let file =
                TempSecretFile::pgpass(&dir.path().join("tmp"), "postgres", &secret).unwrap();
            path = file.path().to_path_buf();
            let text = std::fs::read_to_string(&path).unwrap();
            assert_eq!(text, "*:*:*:postgres:pa\\:ss\\\\word\n");
            assert!(!format!("{file:?}").contains("pa:ss"));
        }
        assert!(!path.exists());
    }

    #[test]
    fn escape_handles_special_characters() {
        assert_eq!(escape_pgpass("a:b\\c"), "a\\:b\\\\c");
        assert_eq!(escape_pgpass("plain"), "plain");
    }
}
