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
    /// Display-only unified diff text (`---`/`+++` headers, one hunk).
    /// Configuration values are redacted and JSON/TOML may be normalized;
    /// this is not an exact patch to apply to the original file.
    /// A new file diffs against empty text; a removed file diffs to empty text.
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

const DISPLAY_REDACTION: &str = "[REDACTED:config]";
const DISPLAY_MAX_DEPTH: usize = 64;

fn normalized_key(key: &str) -> String {
    key.chars()
        .filter(char::is_ascii_alphanumeric)
        .map(|character| character.to_ascii_lowercase())
        .collect()
}

fn sensitive_field(key: &str) -> bool {
    [
        "password",
        "passwd",
        "secret",
        "token",
        "apikey",
        "privatekey",
        "credential",
        "credentials",
    ]
    .iter()
    .any(|suffix| key.ends_with(suffix))
        || matches!(
            key,
            "authorization" | "proxyauthorization" | "cookie" | "setcookie"
        )
}

fn sensitive_map(key: &str) -> bool {
    matches!(
        key,
        "env" | "environment" | "envvars" | "headers" | "httpheaders"
    )
}

fn environment_name(name: &str) -> bool {
    let mut characters = name.chars();
    characters
        .next()
        .is_some_and(|first| first.is_ascii_alphabetic() || first == '_')
        && characters.all(|character| character.is_ascii_alphanumeric() || character == '_')
}

fn mask_json_payload(value: &mut serde_json::Value, depth: usize) {
    if depth >= DISPLAY_MAX_DEPTH {
        *value = serde_json::Value::String(DISPLAY_REDACTION.to_owned());
        return;
    }
    match value {
        serde_json::Value::Object(values) => {
            for value in values.values_mut() {
                mask_json_payload(value, depth + 1);
            }
        }
        serde_json::Value::Array(values) => {
            for value in values {
                mask_json_payload(value, depth + 1);
            }
        }
        _ => *value = serde_json::Value::String(DISPLAY_REDACTION.to_owned()),
    }
}

fn safe_json(value: &mut serde_json::Value, depth: usize) {
    if depth >= DISPLAY_MAX_DEPTH {
        *value = serde_json::Value::String(DISPLAY_REDACTION.to_owned());
        return;
    }
    match value {
        serde_json::Value::Object(values) => {
            for (key, value) in values {
                let key = normalized_key(key);
                let names = key == "envvars"
                    && value.as_array().is_some_and(|values| {
                        values
                            .iter()
                            .all(|value| value.as_str().is_some_and(environment_name))
                    });
                if sensitive_field(&key) || (sensitive_map(&key) && !names) {
                    mask_json_payload(value, depth + 1);
                } else {
                    safe_json(value, depth + 1);
                }
            }
        }
        serde_json::Value::Array(values) => {
            for value in values {
                safe_json(value, depth + 1);
            }
        }
        serde_json::Value::String(text) => *text = knowell_secrets::redact(text).text,
        _ => {}
    }
}

fn mask_toml_payload(value: &mut toml::Value, depth: usize) {
    if depth >= DISPLAY_MAX_DEPTH {
        *value = toml::Value::String(DISPLAY_REDACTION.to_owned());
        return;
    }
    match value {
        toml::Value::Table(values) => {
            for (_, value) in values.iter_mut() {
                mask_toml_payload(value, depth + 1);
            }
        }
        toml::Value::Array(values) => {
            for value in values {
                mask_toml_payload(value, depth + 1);
            }
        }
        _ => *value = toml::Value::String(DISPLAY_REDACTION.to_owned()),
    }
}

fn safe_toml(value: &mut toml::Value, depth: usize) {
    if depth >= DISPLAY_MAX_DEPTH {
        *value = toml::Value::String(DISPLAY_REDACTION.to_owned());
        return;
    }
    match value {
        toml::Value::Table(values) => {
            for (key, value) in values {
                let key = normalized_key(key);
                let names = key == "envvars"
                    && value.as_array().is_some_and(|values| {
                        values
                            .iter()
                            .all(|value| value.as_str().is_some_and(environment_name))
                    });
                if sensitive_field(&key) || (sensitive_map(&key) && !names) {
                    mask_toml_payload(value, depth + 1);
                } else {
                    safe_toml(value, depth + 1);
                }
            }
        }
        toml::Value::Array(values) => {
            for value in values {
                safe_toml(value, depth + 1);
            }
        }
        toml::Value::String(text) => *text = knowell_secrets::redact(text).text,
        _ => {}
    }
}

/// The complete document supplies configuration context even when the changed
/// hunk is far from its environment/header table. Parser failures never return
/// original configuration bytes. Only this display copy is normalized.
fn display_text(path: &Path, text: &str) -> String {
    if text.is_empty() {
        return String::new();
    }
    let extension = path
        .extension()
        .and_then(|extension| extension.to_str())
        .unwrap_or("");
    let normalized = if extension.eq_ignore_ascii_case("json") {
        match serde_json::from_str::<serde_json::Value>(text) {
            Ok(mut value) => {
                safe_json(&mut value, 0);
                serde_json::to_string_pretty(&value)
                    .unwrap_or_else(|_| DISPLAY_REDACTION.to_owned())
            }
            Err(_) => DISPLAY_REDACTION.to_owned(),
        }
    } else if extension.eq_ignore_ascii_case("toml") {
        match toml::from_str::<toml::Value>(text) {
            Ok(mut value) => {
                safe_toml(&mut value, 0);
                toml::to_string_pretty(&value).unwrap_or_else(|_| DISPLAY_REDACTION.to_owned())
            }
            Err(_) => DISPLAY_REDACTION.to_owned(),
        }
    } else {
        text.to_owned()
    };
    knowell_secrets::redact(&normalized).text
}

/// Display-safe single-hunk diff: sanitize complete documents first, trim the
/// common head and tail, then keep up to two context lines on each side.
pub(crate) fn unified_diff(path: &Path, before: &str, after: &str) -> String {
    let before = display_text(path, before);
    let after = display_text(path, after);
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
    knowell_secrets::redact(&out).text
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
    fn json_display_masks_full_configuration_context_and_decoded_strings() {
        let escaped_key = format!("ghp_{}", "\\u0041".repeat(36));
        let before = format!(
            r#"{{"mcpServers":{{"other":{{"command":"keep-command","args":["--literal","keep-argv"],"env":{{"SHORT":"zzq","ESCAPED":"CANARY_\u0045NV"}},"environment":{{"LOW_ENTROPY":"aaaaaaaa"}},"env_vars":{{"VALUE":"bbx"}},"headers":{{"Authorization":"ccc","X-Other":"ddd"}},"http_headers":{{"Anything":"eee"}},"apiKey":"fff","password":"ggg","access_token":"hhh","description":"{escaped_key}"}}}}}}"#
        );
        let mut after: serde_json::Value = serde_json::from_str(&before).unwrap();
        after["mcpServers"]["other"]["args"] = serde_json::json!(["--literal", "updated-argv"]);
        let diff = unified_diff(
            Path::new("client.json"),
            &before,
            &serde_json::to_string(&after).unwrap(),
        );
        for value in [
            "zzq",
            "CANARY_ENV",
            "aaaaaaaa",
            "bbx",
            "ccc",
            "ddd",
            "eee",
            "fff",
            "ggg",
            "hhh",
            escaped_key.as_str(),
        ] {
            assert!(
                !diff.contains(value),
                "display leaked a synthetic classified value"
            );
        }
        assert!(!diff.contains(&format!("ghp_{}", "A".repeat(36))));
        assert!(diff.contains("keep-argv") && diff.contains("updated-argv"));
        let shown = display_text(Path::new("client.json"), &before);
        assert!(shown.contains(DISPLAY_REDACTION));
        assert!(shown.contains("keep-command") && shown.contains("--literal"));
    }

    #[test]
    fn toml_display_masks_tables_literals_inlines_and_preserves_name_arrays() {
        let before = "[mcp_servers.other]\ncommand = 'keep-command'\nargs = ['keep-argv']\nenv_vars = ['CANARY_ONE', 'CANARY_TWO']\napi_key = 'zzq'\nenv = { SHORT = 'bbx', ESCAPED = \"CANARY_\\u0045NV\" }\nheaders = { Authorization = 'ccc', 'X-Other' = 'ddd' }\n[mcp_servers.other.environment]\nVALUE = 'aaaaaaaa'\n[mcp_servers.other.http_headers]\nAuthorization = 'eee'\nAny = 'fff'\n";
        let shown = display_text(Path::new("client.toml"), before);
        for value in [
            "zzq",
            "bbx",
            "CANARY_ENV",
            "ccc",
            "ddd",
            "aaaaaaaa",
            "eee",
            "fff",
        ] {
            assert!(
                !shown.contains(value),
                "display leaked a synthetic classified value"
            );
        }
        assert!(shown.contains("CANARY_ONE") && shown.contains("CANARY_TWO"));
        assert!(shown.contains("keep-command") && shown.contains("keep-argv"));
        let after = before.replace("'fff'", "'changedclassifiedvalue'");
        assert!(
            !unified_diff(Path::new("client.toml"), before, &after)
                .contains("changedclassifiedvalue")
        );
    }

    #[test]
    fn malformed_configuration_has_no_raw_display_fallback() {
        let hostile = "KNOWELL_CANARY_unparsed_config";
        for filename in ["client.json", "client.toml"] {
            let diff = unified_diff(Path::new(filename), hostile, &format!("{hostile} changed"));
            assert!(!diff.contains(hostile));
            assert_eq!(
                display_text(Path::new(filename), hostile),
                DISPLAY_REDACTION
            );
        }
    }

    #[test]
    fn markdown_scan_covers_entire_private_key_before_hunk_trimming() {
        let body = "KNOWELLCANARYPRIVATEKEYBASE64ONLY";
        let before = format!(
            "-----BEGIN PRIVATE KEY-----\n{body}\n-----END PRIVATE KEY-----\n\nold instruction\n"
        );
        let after = before.replace("old instruction", "new instruction");
        let diff = unified_diff(Path::new("CLAUDE.md"), &before, &after);
        assert!(!diff.contains(body));
        assert!(diff.contains("old instruction") && diff.contains("new instruction"));
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
