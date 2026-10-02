//! Small file helpers shared by the commands.

use std::io::Write as _;
use std::path::Path;

use anyhow::Context;

/// Writes `contents` to `path` through a temporary file in the same
/// directory and a rename, so readers never see a half-written file.
/// Parent directories are created.
pub(crate) fn write_atomic(path: &Path, contents: &str) -> anyhow::Result<()> {
    let dir = match path.parent().filter(|p| !p.as_os_str().is_empty()) {
        Some(dir) => dir.to_path_buf(),
        None => std::env::current_dir().context("cannot read the current directory")?,
    };
    std::fs::create_dir_all(&dir).with_context(|| format!("cannot create {}", dir.display()))?;
    let mut tmp = tempfile::Builder::new()
        .prefix(".know-")
        .suffix(".tmp")
        .tempfile_in(&dir)
        .with_context(|| format!("cannot create a temporary file in {}", dir.display()))?;
    tmp.write_all(contents.as_bytes())
        .and_then(|()| tmp.as_file().sync_all())
        .with_context(|| format!("cannot write {}", path.display()))?;
    tmp.persist(path)
        .map_err(|e| e.error)
        .with_context(|| format!("cannot write {}", path.display()))?;
    Ok(())
}

/// Reads a UTF-8 file; `Ok(None)` when it does not exist.
pub(crate) fn read_optional(path: &Path) -> anyhow::Result<Option<String>> {
    match std::fs::read_to_string(path) {
        Ok(text) => Ok(Some(text)),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(err) => Err(err).with_context(|| format!("cannot read {}", path.display())),
    }
}

/// `path` with `/` separators, for display in generated files.
pub(crate) fn slash_path(path: &Path) -> String {
    path.to_string_lossy().replace('\\', "/")
}
