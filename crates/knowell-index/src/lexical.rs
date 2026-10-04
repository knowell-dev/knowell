//! Per-view, per-generation Tantivy indexes on disk.
//!
//! Layout: `<data_dir>/lexical/<view-id>/<generation>/`, one document per
//! file (id and path = the project-relative path, body = redacted text). A
//! generation directory is usable once it holds the `KNOWELL_COMPLETE`
//! marker, written after Tantivy's commit.
//!
//! # Incremental builds: copy-on-write
//!
//! Generation `g` is built by copying the complete directory of the active
//! generation, then deleting the documents of changed and removed paths and
//! adding the new versions. Immutable segment components are hard-linked
//! when supported; mutable metadata is copied. On unsupported filesystems
//! segment components are copied, with a diagnostic counter. Queries on
//! the active generation are unaffected while `g` builds. When no complete
//! base exists (first build, data directory lost, base garbage-collected)
//! the index is rebuilt from the store's redacted text instead — slower,
//! same result.
//!
//! # Atomic switch
//!
//! Readers open the directory of the generation the store says is active.
//! Activation in the store is one transaction, and the directory of the new
//! generation is complete before it, so the switch is atomic. Handles
//! cached in this process are replaced after activation; readers holding
//! the previous handle keep using it until they drop it.
//!
//! # Garbage collection
//!
//! After an activation, directories other than the active one, the one
//! building and [`crate::Retention::lexical_previous`] older ones are
//! deleted. A directory that cannot be deleted (Windows refuses while a
//! reader still maps its files) is retried at the next activation.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, PoisonError};

use knowell_lexical::{LexicalDoc, LexicalIndex};
use knowell_store::ViewId;

use crate::error::IndexError;

const COMPLETE_MARKER: &str = "KNOWELL_COMPLETE";

/// Changes to apply to a base index (or the documents of a fresh one).
#[derive(Debug, Default)]
pub(crate) struct LexicalUpdate {
    /// Document ids (paths) to remove.
    pub(crate) deletes: Vec<String>,
    /// Documents to add (path, redacted text); an existing document with the
    /// same path is replaced.
    pub(crate) adds: Vec<(String, Arc<str>)>,
}

/// The lexical indexes of every view.
pub(crate) struct LexicalStore {
    root: PathBuf,
    open: Mutex<BTreeMap<ViewId, (i64, Arc<LexicalIndex>)>>,
}

impl LexicalStore {
    pub(crate) fn new(data_dir: &Path) -> Self {
        Self {
            root: data_dir.join("lexical"),
            open: Mutex::new(BTreeMap::new()),
        }
    }

    pub(crate) fn view_dir(&self, view: ViewId) -> PathBuf {
        self.root.join(view.to_string())
    }

    pub(crate) fn dir(&self, view: ViewId, generation: i64) -> PathBuf {
        self.view_dir(view).join(generation.to_string())
    }

    /// Whether generation `g` of `view` has a complete index on disk.
    pub(crate) fn is_complete(&self, view: ViewId, generation: i64) -> bool {
        self.dir(view, generation).join(COMPLETE_MARKER).is_file()
    }

    /// Builds generation `generation` (blocking). With `base`, copies that
    /// generation's complete index and applies `update`; without, creates a
    /// fresh index holding `update.adds`. Returns the number of documents.
    pub(crate) fn build(
        &self,
        view: ViewId,
        generation: i64,
        base: Option<i64>,
        update: &LexicalUpdate,
    ) -> Result<u64, IndexError> {
        let dir = self.dir(view, generation);
        // A previous attempt of this generation may have left a partial
        // directory behind.
        if dir.exists() {
            std::fs::remove_dir_all(&dir)
                .map_err(|e| IndexError::io("removing an incomplete lexical index", e))?;
        }
        let index = match base {
            Some(base) => {
                copy_index(&self.dir(view, base), &dir)?;
                LexicalIndex::open_in_dir(&dir)?
            }
            None => LexicalIndex::create_in_dir(&dir)?,
        };
        let mut writer = index.writer()?;
        if base.is_some() {
            for id in &update.deletes {
                writer.delete(id)?;
            }
            for (path, _) in &update.adds {
                writer.delete(path)?;
            }
        }
        for (path, text) in &update.adds {
            writer.add(LexicalDoc {
                id: path,
                path,
                text,
            })?;
        }
        writer.commit()?;
        let docs = index.num_docs();
        drop(index);
        std::fs::write(dir.join(COMPLETE_MARKER), generation.to_string())
            .map_err(|e| IndexError::io("marking a lexical index complete", e))?;
        Ok(docs)
    }

    /// The index of `generation`, opened (and cached) if complete.
    pub(crate) fn get(
        &self,
        view: ViewId,
        generation: i64,
    ) -> Result<Option<Arc<LexicalIndex>>, IndexError> {
        if let Some((g, index)) = self.lock().get(&view)
            && *g == generation
        {
            return Ok(Some(Arc::clone(index)));
        }
        if !self.is_complete(view, generation) {
            return Ok(None);
        }
        let index = Arc::new(LexicalIndex::open_in_dir(&self.dir(view, generation))?);
        let mut open = self.lock();
        // Keep the newest generation cached when two callers race.
        let replace = open.get(&view).is_none_or(|(g, _)| *g <= generation);
        if replace {
            open.insert(view, (generation, Arc::clone(&index)));
        }
        Ok(Some(index))
    }

    /// Drops the cached handle of `view` (its files can then be deleted once
    /// no reader holds them).
    #[cfg(test)]
    pub(crate) fn forget(&self, view: ViewId) {
        self.lock().remove(&view);
    }

    /// Deletes generation directories of `view` other than `keep`. Returns
    /// how many were deleted; failures are logged and retried later.
    pub(crate) fn collect_garbage(&self, view: ViewId, keep: &[i64]) -> usize {
        let Ok(entries) = std::fs::read_dir(self.view_dir(view)) else {
            return 0;
        };
        let mut removed = 0;
        for entry in entries.flatten() {
            let name = entry.file_name();
            let Some(generation) = name.to_str().and_then(|n| n.parse::<i64>().ok()) else {
                continue;
            };
            if keep.contains(&generation) {
                continue;
            }
            match std::fs::remove_dir_all(entry.path()) {
                Ok(()) => removed += 1,
                Err(error) => {
                    tracing::debug!(%view, generation, %error, "lexical index still in use; retrying later");
                }
            }
        }
        removed
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, BTreeMap<ViewId, (i64, Arc<LexicalIndex>)>> {
        self.open.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

/// Shares only immutable Tantivy segment components. Metadata, locks and
/// completion markers must never share writable storage between generations.
fn immutable_segment(name: &std::ffi::OsStr) -> bool {
    let Some(name) = name.to_str() else {
        return false;
    };
    let mut parts = name.split('.');
    let Some(id) = parts.next() else {
        return false;
    };
    if id.len() != 32 || !id.bytes().all(|b| b.is_ascii_hexdigit()) {
        return false;
    }
    match (parts.next(), parts.next(), parts.next()) {
        (Some("fast" | "fieldnorm" | "idx" | "pos" | "store" | "term"), None, None) => true,
        (Some(stamp), Some("del"), None) => {
            !stamp.is_empty() && stamp.bytes().all(|b| b.is_ascii_digit())
        }
        _ => false,
    }
}

/// Copies mutable files and shares immutable segments of a complete index.
fn copy_index(from: &Path, to: &Path) -> Result<(), IndexError> {
    std::fs::create_dir_all(to).map_err(|e| IndexError::io("creating a lexical index", e))?;
    let entries =
        std::fs::read_dir(from).map_err(|e| IndexError::io("reading the base lexical index", e))?;
    let mut linked_bytes = 0u64;
    let mut copied_bytes = 0u64;
    for entry in entries {
        let entry = entry.map_err(|e| IndexError::io("reading the base lexical index", e))?;
        let name = entry.file_name();
        let skip = name
            .to_str()
            .is_some_and(|n| n == COMPLETE_MARKER || n.ends_with(".lock"));
        let is_file = entry
            .file_type()
            .map_err(|e| IndexError::io("reading a lexical file type", e))?
            .is_file();
        if skip || !is_file {
            continue;
        }
        let source = entry.path();
        let destination = to.join(&name);
        if immutable_segment(&name) {
            match std::fs::hard_link(&source, &destination) {
                Ok(()) => {
                    linked_bytes = linked_bytes.saturating_add(
                        entry
                            .metadata()
                            .map_err(|e| IndexError::io("reading a lexical segment size", e))?
                            .len(),
                    );
                    continue;
                }
                Err(error) => {
                    tracing::debug!(error_kind = ?error.kind(), "lexical segment sharing unavailable; copying")
                }
            }
        }
        copied_bytes = copied_bytes.saturating_add(
            std::fs::copy(source, destination)
                .map_err(|e| IndexError::io("copying the base lexical index", e))?,
        );
    }
    tracing::debug!(
        linked_bytes,
        copied_bytes,
        "prepared lexical generation files"
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text(s: &str) -> Arc<str> {
        Arc::from(s)
    }

    #[test]
    fn only_known_immutable_segment_names_are_shared() {
        let id = "0123456789abcdef0123456789abcdef";
        for ext in ["fast", "fieldnorm", "idx", "pos", "store", "term", "42.del"] {
            assert!(immutable_segment(std::ffi::OsStr::new(&format!(
                "{id}.{ext}"
            ))));
        }
        for name in [
            "meta.json",
            ".managed.json",
            "writer.lock",
            "KNOWELL_COMPLETE",
            "unknown.fast",
            "../meta.json",
        ] {
            assert!(!immutable_segment(std::ffi::OsStr::new(name)));
        }
        for ext in ["json", "del", "bad.del", "42.del.extra", "store.extra"] {
            assert!(!immutable_segment(std::ffi::OsStr::new(&format!(
                "{id}.{ext}"
            ))));
        }
    }

    #[test]
    fn generation_metadata_and_completion_are_isolated() {
        let directory = tempfile::tempdir().unwrap();
        let base = directory.path().join("base");
        let next = directory.path().join("next");
        std::fs::create_dir_all(&base).unwrap();
        let component = "0123456789abcdef0123456789abcdef.store";
        std::fs::write(base.join(component), b"immutable body").unwrap();
        std::fs::write(base.join("meta.json"), b"old metadata").unwrap();
        std::fs::write(base.join(".managed.json"), b"old management").unwrap();
        std::fs::write(base.join(COMPLETE_MARKER), b"complete").unwrap();
        std::fs::write(base.join("writer.lock"), b"lock").unwrap();
        copy_index(&base, &next).unwrap();
        std::fs::write(next.join("meta.json"), b"new metadata").unwrap();
        std::fs::write(next.join(".managed.json"), b"new management").unwrap();
        assert_eq!(
            std::fs::read(base.join("meta.json")).unwrap(),
            b"old metadata"
        );
        assert_eq!(
            std::fs::read(base.join(".managed.json")).unwrap(),
            b"old management"
        );
        assert!(!next.join(COMPLETE_MARKER).exists());
        assert!(!next.join("writer.lock").exists());
        std::fs::remove_file(base.join(component)).unwrap();
        assert_eq!(
            std::fs::read(next.join(component)).unwrap(),
            b"immutable body"
        );
    }

    #[test]
    fn copy_on_write_keeps_the_base_untouched() {
        let dir = tempfile::tempdir().unwrap();
        let store = LexicalStore::new(dir.path());
        let view = ViewId(uuid::Uuid::nil());
        let first = LexicalUpdate {
            deletes: Vec::new(),
            adds: vec![
                ("a.rs".into(), text("fn alpha_handler() {}")),
                ("b.rs".into(), text("fn beta_handler() {}")),
            ],
        };
        assert_eq!(store.build(view, 1, None, &first).unwrap(), 2);
        assert!(store.is_complete(view, 1));

        let second = LexicalUpdate {
            deletes: vec!["b.rs".into()],
            adds: vec![
                ("a.rs".into(), text("fn gamma_handler() {}")),
                ("c.rs".into(), text("fn delta_handler() {}")),
            ],
        };
        assert_eq!(store.build(view, 2, Some(1), &second).unwrap(), 2);

        let g1 = store.get(view, 1).unwrap().unwrap();
        assert_eq!(g1.search("beta", 5).unwrap().len(), 1);
        assert_eq!(g1.search("gamma", 5).unwrap().len(), 0);
        let g2 = store.get(view, 2).unwrap().unwrap();
        assert_eq!(g2.search("beta", 5).unwrap().len(), 0);
        assert_eq!(g2.search("alpha", 5).unwrap().len(), 0);
        let hits = g2.search("gamma", 5).unwrap();
        assert_eq!(hits.first().map(|h| h.id.as_str()), Some("a.rs"));
        assert!(store.get(view, 3).unwrap().is_none());

        drop((g1, g2));
        store.forget(view);
        assert_eq!(store.collect_garbage(view, &[2]), 1);
        assert!(!store.dir(view, 1).exists());
        assert!(store.is_complete(view, 2));
    }

    #[test]
    fn rebuilding_a_generation_replaces_a_partial_one() {
        let dir = tempfile::tempdir().unwrap();
        let store = LexicalStore::new(dir.path());
        let view = ViewId(uuid::Uuid::nil());
        std::fs::create_dir_all(store.dir(view, 1)).unwrap();
        std::fs::write(store.dir(view, 1).join("garbage"), b"x").unwrap();
        assert!(!store.is_complete(view, 1));
        let update = LexicalUpdate {
            deletes: Vec::new(),
            adds: vec![("a.md".into(), text("hello world"))],
        };
        assert_eq!(store.build(view, 1, None, &update).unwrap(), 1);
        assert!(store.is_complete(view, 1));
    }
}
