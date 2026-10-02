//! File-edit plumbing shared by `connect` and `ci`: reading, diffing, marker
//! blocks, backups and (optionally dry-run) writes.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use crate::error::SetupError;

/// Suffix of the backup written before a pre-existing file is modified.
pub const BACKUP_SUFFIX: &str = ".knowell-bak";

/// Unified diff of one file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileDiff {
    /// The file the diff applies to.
    pub path: PathBuf,
    /// Unified diff text (`---`/`+++` headers, one hunk). A new file diffs
    /// against empty text; a removed file diffs to empty text.
    pub diff: String,
}

/// Result of a setup operation that edits files.
///
/// With `dry_run` nothing is written, but the fields describe exactly what
/// a real run would do.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ConnectReport {
    /// Files created, modified or removed (empty when everything was already
    /// in the requested state).
    pub changed_files: Vec<PathBuf>,
    /// One diff per changed file.
    pub diffs: Vec<FileDiff>,
    /// Backups written (`<file>.knowell-bak`); only for files that existed
    /// and only when no backup was there yet, so the first original survives.
    pub backups: Vec<PathBuf>,
    /// Human-readable remarks: what was skipped, caveats, next steps.
    pub notes: Vec<String>,
}

/// A planned change of one file; `after == None` means "file absent".
#[derive(Debug)]
pub(crate) struct Edit {
    pub(crate) path: PathBuf,
    pub(crate) before: Option<String>,
    pub(crate) after: Option<String>,
}

/// Reads a UTF-8 file, `None` when it does not exist.
pub(crate) fn read_optional(path: &Path) -> Result<Option<String>, SetupError> {
    match fs::read_to_string(path) {
        Ok(text) => Ok(Some(text)),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(source) => Err(SetupError::Read {
            path: path.to_path_buf(),
            source,
        }),
    }
}

/// Reads `path` and applies `transform` to its text to plan an [`Edit`].
pub(crate) fn plan_file(
    path: PathBuf,
    transform: impl FnOnce(Option<&str>) -> Result<Option<String>, SetupError>,
) -> Result<Edit, SetupError> {
    let before = read_optional(&path)?;
    let after = transform(before.as_deref())?;
    Ok(Edit {
        path,
        before,
        after,
    })
}

/// Applies edits (or only reports them when `dry_run`). All edits are
/// planned before any is applied, so a failure while planning changes nothing.
pub(crate) fn commit(
    edits: Vec<Edit>,
    dry_run: bool,
    report: &mut ConnectReport,
) -> Result<(), SetupError> {
    for edit in edits {
        if edit.before == edit.after {
            continue;
        }
        let before = edit.before.as_deref().unwrap_or("");
        let after = edit.after.as_deref().unwrap_or("");
        report.diffs.push(FileDiff {
            path: edit.path.clone(),
            diff: unified_diff(&edit.path, before, after),
        });
        report.changed_files.push(edit.path.clone());
        let backup = backup_path(&edit.path);
        let wants_backup = edit.before.is_some() && !backup.exists();
        if wants_backup {
            report.backups.push(backup.clone());
        }
        if dry_run {
            continue;
        }
        if wants_backup {
            fs::copy(&edit.path, &backup).map_err(|source| SetupError::Write {
                path: backup.clone(),
                source,
            })?;
        }
        match &edit.after {
            Some(text) => write_atomic(&edit.path, text)?,
            None => fs::remove_file(&edit.path).map_err(|source| SetupError::Write {
                path: edit.path.clone(),
                source,
            })?,
        }
    }
    Ok(())
}

/// `<file>.knowell-bak` next to `path`.
pub(crate) fn backup_path(path: &Path) -> PathBuf {
    let mut name = path
        .file_name()
        .map(|n| n.to_os_string())
        .unwrap_or_default();
    name.push(BACKUP_SUFFIX);
    path.with_file_name(name)
}

fn write_atomic(path: &Path, text: &str) -> Result<(), SetupError> {
    let werr = |source| SetupError::Write {
        path: path.to_path_buf(),
        source,
    };
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(werr)?;
    }
    let mut tmp_name = path
        .file_name()
        .map(|n| n.to_os_string())
        .unwrap_or_default();
    tmp_name.push(".knowell-tmp");
    let tmp = path.with_file_name(tmp_name);
    fs::write(&tmp, text).map_err(werr)?;
    fs::rename(&tmp, path).map_err(|source| {
        let _ = fs::remove_file(&tmp);
        werr(source)
    })
}

/// Single-hunk unified diff: the common head and tail are trimmed, up to two
/// lines of context are kept on each side.
pub(crate) fn unified_diff(path: &Path, before: &str, after: &str) -> String {
    let old: Vec<&str> = before.lines().collect();
    let new: Vec<&str> = after.lines().collect();
    let mut head = 0;
    while let (Some(a), Some(b)) = (old.get(head), new.get(head)) {
        if a != b {
            break;
        }
        head += 1;
    }
    let mut tail = 0;
    while tail + head < old.len() && tail + head < new.len() {
        let a = old.get(old.len() - 1 - tail);
        let b = new.get(new.len() - 1 - tail);
        if a != b {
            break;
        }
        tail += 1;
    }
    let ctx_before = head.min(2);
    let ctx_after = tail.min(2);
    let start = head - ctx_before;
    let old_end = old.len() - tail + ctx_after;
    let new_end = new.len() - tail + ctx_after;
    let old_slice = old.get(start..old_end).unwrap_or(&[]);
    let new_slice = new.get(start..new_end).unwrap_or(&[]);

    let mut out = format!(
        "--- {}\n+++ {}\n@@ -{},{} +{},{} @@\n",
        path.display(),
        path.display(),
        start + 1,
        old_slice.len(),
        start + 1,
        new_slice.len()
    );
    for line in old.get(start..head).unwrap_or(&[]) {
        out.push(' ');
        out.push_str(line);
        out.push('\n');
    }
    for line in old.get(head..old.len() - tail).unwrap_or(&[]) {
        out.push('-');
        out.push_str(line);
        out.push('\n');
    }
    for line in new.get(head..new.len() - tail).unwrap_or(&[]) {
        out.push('+');
        out.push_str(line);
        out.push('\n');
    }
    for line in old
        .get(old.len() - tail..old.len() - tail + ctx_after)
        .unwrap_or(&[])
    {
        out.push(' ');
        out.push_str(line);
        out.push('\n');
    }
    out
}

/// Begin/end marker lines of a block Knowell owns inside a user file.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Markers {
    pub(crate) begin: &'static str,
    pub(crate) end: &'static str,
}

/// Markers for Markdown files.
pub(crate) const MD_MARKERS: Markers = Markers {
    begin: "<!-- knowell:begin connect -->",
    end: "<!-- knowell:end connect -->",
};

/// Markers for TOML files.
pub(crate) const TOML_MARKERS: Markers = Markers {
    begin: "# knowell:begin connect",
    end: "# knowell:end connect",
};

/// Byte range of the block including the end marker's line break.
fn find_block(text: &str, m: Markers) -> Result<Option<(usize, usize)>, ()> {
    let Some(start) = text.find(m.begin) else {
        return if text.contains(m.end) {
            Err(())
        } else {
            Ok(None)
        };
    };
    let after_begin = start + m.begin.len();
    let Some(rel_end) = text.get(after_begin..).and_then(|t| t.find(m.end)) else {
        return Err(());
    };
    let mut end = after_begin + rel_end + m.end.len();
    if text.get(end..).is_some_and(|t| t.starts_with("\r\n")) {
        end += 2;
    } else if text.get(end..).is_some_and(|t| t.starts_with('\n')) {
        end += 1;
    }
    Ok(Some((start, end)))
}

/// Whether the text contains an intact block.
pub(crate) fn has_block(text: &str, m: Markers) -> bool {
    matches!(find_block(text, m), Ok(Some(_)))
}

/// Inserts or replaces the block (`block` must start with the begin marker
/// and end with the end marker). Unbalanced markers are a conflict.
pub(crate) fn upsert_block(
    path: &Path,
    text: &str,
    block: &str,
    m: Markers,
) -> Result<String, SetupError> {
    match find_block(text, m) {
        Err(()) => Err(unbalanced(path)),
        Ok(Some((start, end))) => {
            let head = text.get(..start).unwrap_or("");
            let tail = text.get(end..).unwrap_or("");
            Ok(format!("{head}{block}\n{tail}"))
        }
        Ok(None) => {
            let mut out = text.to_owned();
            if !out.is_empty() {
                if !out.ends_with('\n') {
                    out.push('\n');
                }
                out.push('\n');
            }
            out.push_str(block);
            out.push('\n');
            Ok(out)
        }
    }
}

/// Removes the block and the separating blank line [`upsert_block`] added.
/// Returns `None` when there is no block.
pub(crate) fn remove_block(
    path: &Path,
    text: &str,
    m: Markers,
) -> Result<Option<String>, SetupError> {
    match find_block(text, m) {
        Err(()) => Err(unbalanced(path)),
        Ok(None) => Ok(None),
        Ok(Some((start, end))) => {
            let mut head = text.get(..start).unwrap_or("").to_owned();
            if head.ends_with("\n\n") {
                head.pop();
            }
            let tail = text.get(end..).unwrap_or("");
            Ok(Some(format!("{head}{tail}")))
        }
    }
}

fn unbalanced(path: &Path) -> SetupError {
    SetupError::Conflict {
        path: path.to_path_buf(),
        what: "a knowell begin/end marker is present without its partner".to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn block_roundtrip_restores_original() {
        let p = Path::new("x.md");
        for original in ["", "a\n", "a", "a\n\nb\n"] {
            let block = format!("{}\nbody\n{}", MD_MARKERS.begin, MD_MARKERS.end);
            let with = upsert_block(p, original, &block, MD_MARKERS).unwrap();
            assert!(has_block(&with, MD_MARKERS));
            let again = upsert_block(p, &with, &block, MD_MARKERS).unwrap();
            assert_eq!(with, again, "idempotent");
            let without = remove_block(p, &with, MD_MARKERS).unwrap().unwrap();
            let expected = if original.is_empty() || original.ends_with('\n') {
                original.to_owned()
            } else {
                format!("{original}\n")
            };
            assert_eq!(without, expected);
        }
    }

    #[test]
    fn block_is_replaced_in_place() {
        let p = Path::new("x.md");
        let b1 = format!("{}\none\n{}", MD_MARKERS.begin, MD_MARKERS.end);
        let b2 = format!("{}\ntwo\n{}", MD_MARKERS.begin, MD_MARKERS.end);
        let text = format!("top\n\n{b1}\nbottom\n");
        let out = upsert_block(p, &text, &b2, MD_MARKERS).unwrap();
        assert_eq!(out, format!("top\n\n{b2}\nbottom\n"));
    }

    #[test]
    fn unbalanced_markers_are_conflicts() {
        let p = Path::new("x.md");
        let text = format!("a\n{}\nno end\n", MD_MARKERS.begin);
        assert!(matches!(
            upsert_block(p, &text, "x", MD_MARKERS),
            Err(SetupError::Conflict { .. })
        ));
        assert!(remove_block(p, MD_MARKERS.end, MD_MARKERS).is_err());
    }

    #[test]
    fn diff_shows_only_changed_lines_with_context() {
        let d = unified_diff(Path::new("f"), "a\nb\nc\nd\ne\n", "a\nb\nX\nd\ne\n");
        assert!(d.contains("-c\n+X\n"), "{d}");
        assert!(d.contains(" b\n") && d.contains(" d\n"));
        let created = unified_diff(Path::new("f"), "", "one\ntwo\n");
        assert!(created.contains("+one\n+two\n"));
        let removed = unified_diff(Path::new("f"), "one\n", "");
        assert!(removed.contains("-one\n"));
    }

    #[test]
    fn backups_are_written_once_and_dry_run_writes_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("c.json");
        fs::write(&file, "original").unwrap();
        let mk = |after: &str| Edit {
            path: file.clone(),
            before: read_optional(&file).unwrap(),
            after: Some(after.to_owned()),
        };
        let mut r = ConnectReport::default();
        commit(vec![mk("v1")], true, &mut r).unwrap();
        assert_eq!(fs::read_to_string(&file).unwrap(), "original");
        assert_eq!(r.changed_files.len(), 1);
        assert!(!backup_path(&file).exists());

        let mut r = ConnectReport::default();
        commit(vec![mk("v1")], false, &mut r).unwrap();
        assert_eq!(fs::read_to_string(backup_path(&file)).unwrap(), "original");
        let mut r = ConnectReport::default();
        commit(vec![mk("v2")], false, &mut r).unwrap();
        assert!(r.backups.is_empty());
        assert_eq!(fs::read_to_string(backup_path(&file)).unwrap(), "original");
    }
}
