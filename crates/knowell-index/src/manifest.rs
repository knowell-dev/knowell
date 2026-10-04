//! Per-generation manifests on disk: which paths a generation indexed, the
//! git blob each came from and the content hash it produced (or why it was
//! skipped).
//!
//! The manifest is an accelerator and a reconciliation reference, never the
//! source of truth (the store is):
//!
//! - **Blob cache.** A full re-walk after a force-push lists the new tree
//!   (cheap) and reads only blobs whose id is not in the previous manifest,
//!   so unchanged files are neither read nor redacted again.
//! - **Reconciliation.** Its tree hash must equal the tree hash of the
//!   store's files for the same generation; a mismatch (lost rows, a store
//!   restored from an older backup) schedules a full rebuild.
//!
//! A missing or unreadable manifest only costs speed: the next build reads
//! every blob again.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use knowell_core::{ContentHash, RepoPath};
use knowell_store::ViewId;
use serde::{Deserialize, Serialize};

use crate::error::IndexError;

/// Format version of the manifest file.
const FORMAT: u32 = 1;

/// What happened to one path of a generation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub(crate) enum EntryState {
    /// Indexed with this content hash.
    Indexed {
        /// BLAKE3 of the original bytes.
        hash: ContentHash,
    },
    /// Not indexed for a reason that depends on the blob only (too large,
    /// binary, not UTF-8), so the decision can be reused for the same blob.
    Skipped {
        /// Stable reason text.
        reason: String,
    },
}

/// One looked-up path. Paths excluded by policy are not listed: exclusion
/// is decided from the path alone and rechecked on every build.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Entry {
    /// Project-relative path.
    pub(crate) path: RepoPath,
    /// Git blob id, `None` for directory sources.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) blob: Option<String>,
    /// Outcome.
    #[serde(flatten)]
    pub(crate) state: EntryState,
}

/// The manifest of one view generation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Manifest {
    pub(crate) format: u32,
    pub(crate) view: ViewId,
    pub(crate) generation: i64,
    /// Commit indexed (`None` for directory sources).
    pub(crate) commit: Option<String>,
    /// Hash of the content policy the generation was built with (excludes,
    /// size limit, project root); a different policy invalidates the cache.
    pub(crate) policy: ContentHash,
    /// T1 syntax evidence policy. Missing in older manifests means the
    /// generation needs syntax-only refresh, without changing content inputs.
    #[serde(default)]
    pub(crate) syntax_policy: u32,
    /// Tree hash of the indexed files (see [`crate::merkle`]).
    pub(crate) tree_hash: ContentHash,
    /// Entries sorted by path.
    pub(crate) entries: Vec<Entry>,
}

impl Manifest {
    pub(crate) fn new(
        view: ViewId,
        generation: i64,
        commit: Option<String>,
        policy: ContentHash,
        mut entries: Vec<Entry>,
    ) -> Self {
        entries.sort_by(|a, b| a.path.cmp(&b.path));
        entries.dedup_by(|later, earlier| later.path == earlier.path);
        let tree_hash = crate::merkle::tree_hash(entries.iter().filter_map(|e| match &e.state {
            EntryState::Indexed { hash } => Some((&e.path, hash)),
            EntryState::Skipped { .. } => None,
        }));
        Self {
            format: FORMAT,
            view,
            generation,
            commit,
            policy,
            syntax_policy: crate::analyze::SYNTAX_POLICY_VERSION,
            tree_hash,
            entries,
        }
    }

    /// Blob id → outcome, for reusing decisions on unchanged blobs.
    pub(crate) fn blob_cache(&self) -> BTreeMap<&str, &EntryState> {
        self.entries
            .iter()
            .filter_map(|e| e.blob.as_deref().map(|b| (b, &e.state)))
            .collect()
    }

    /// Whether unchanged files need current generation-scoped syntax evidence.
    pub(crate) fn needs_syntax_refresh(&self) -> bool {
        self.syntax_policy != crate::analyze::SYNTAX_POLICY_VERSION
    }
}

/// Where manifests of `view` live.
pub(crate) fn view_dir(data_dir: &Path, view: ViewId) -> PathBuf {
    data_dir.join("views").join(view.to_string())
}

fn file_name(generation: i64) -> String {
    format!("manifest-{generation}.json")
}

/// Writes the manifest atomically (temporary file, then rename).
pub(crate) fn save(data_dir: &Path, manifest: &Manifest) -> Result<(), IndexError> {
    let dir = view_dir(data_dir, manifest.view);
    std::fs::create_dir_all(&dir)
        .map_err(|e| IndexError::io("creating the manifest directory", e))?;
    let json =
        serde_json::to_vec(manifest).map_err(|e| IndexError::invalid("manifest", e.to_string()))?;
    let target = dir.join(file_name(manifest.generation));
    let temp = dir.join(format!("{}.tmp", file_name(manifest.generation)));
    std::fs::write(&temp, json).map_err(|e| IndexError::io("writing a manifest", e))?;
    std::fs::rename(&temp, &target).map_err(|e| IndexError::io("replacing a manifest", e))
}

/// Reads the manifest of a generation; `None` when it is missing, unreadable
/// or of another format (the caller then reads every blob again).
pub(crate) fn load(data_dir: &Path, view: ViewId, generation: i64) -> Option<Manifest> {
    let path = view_dir(data_dir, view).join(file_name(generation));
    let bytes = std::fs::read(path).ok()?;
    let manifest: Manifest = serde_json::from_slice(&bytes).ok()?;
    (manifest.format == FORMAT && manifest.view == view && manifest.generation == generation)
        .then_some(manifest)
}

/// Deletes manifests of `view` other than `keep`. Failures are ignored: a
/// stale manifest is only a cache.
pub(crate) fn retain(data_dir: &Path, view: ViewId, keep: &[i64]) {
    let Ok(entries) = std::fs::read_dir(view_dir(data_dir, view)) else {
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(name) = name.to_str() else { continue };
        let generation = name
            .strip_prefix("manifest-")
            .and_then(|rest| {
                rest.strip_suffix(".json")
                    .or_else(|| rest.strip_suffix(".json.tmp"))
            })
            .and_then(|n| n.parse::<i64>().ok());
        if let Some(generation) = generation
            && !keep.contains(&generation)
        {
            let _ = std::fs::remove_file(entry.path());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(s: &str) -> RepoPath {
        RepoPath::new(s).unwrap()
    }

    #[test]
    fn save_load_round_trip_and_retain() {
        let dir = tempfile::tempdir().unwrap();
        let view = ViewId(uuid::Uuid::nil());
        let policy = ContentHash::of(b"policy");
        let entries = vec![
            Entry {
                path: p("b.rs"),
                blob: Some("bb".repeat(20)),
                state: EntryState::Indexed {
                    hash: ContentHash::of(b"b"),
                },
            },
            Entry {
                path: p("a.bin"),
                blob: Some("aa".repeat(20)),
                state: EntryState::Skipped {
                    reason: "binary".into(),
                },
            },
        ];
        let m = Manifest::new(view, 3, Some("cc".repeat(20)), policy, entries);
        assert_eq!(m.entries[0].path.as_str(), "a.bin");
        save(dir.path(), &m).unwrap();
        let back = load(dir.path(), view, 3).unwrap();
        assert_eq!(back, m);
        assert_eq!(back.blob_cache().len(), 2);
        assert!(load(dir.path(), view, 4).is_none());

        let m4 = Manifest::new(view, 4, None, policy, Vec::new());
        save(dir.path(), &m4).unwrap();
        retain(dir.path(), view, &[4]);
        assert!(load(dir.path(), view, 3).is_none());
        assert!(load(dir.path(), view, 4).is_some());
    }

    #[test]
    fn corrupt_manifest_is_ignored() {
        let dir = tempfile::tempdir().unwrap();
        let view = ViewId(uuid::Uuid::nil());
        let vdir = view_dir(dir.path(), view);
        std::fs::create_dir_all(&vdir).unwrap();
        std::fs::write(vdir.join("manifest-1.json"), b"{not json").unwrap();
        assert!(load(dir.path(), view, 1).is_none());
        std::fs::write(vdir.join("manifest-2.json"), b"").unwrap();
        assert!(load(dir.path(), view, 2).is_none());
    }

    #[test]
    fn legacy_manifest_requires_syntax_refresh_without_losing_blob_cache() {
        let dir = tempfile::tempdir().unwrap();
        let view = ViewId(uuid::Uuid::nil());
        let current = Manifest::new(
            view,
            1,
            Some("ab".repeat(20)),
            ContentHash::of(b"policy"),
            vec![Entry {
                path: p("src/tiny.rs"),
                blob: Some("cd".repeat(20)),
                state: EntryState::Indexed {
                    hash: ContentHash::of(b"fn tiny() {}"),
                },
            }],
        );
        assert!(!current.needs_syntax_refresh());
        save(dir.path(), &current).unwrap();
        let manifest_path = view_dir(dir.path(), view).join(file_name(1));
        let mut legacy = serde_json::to_value(&current).unwrap();
        legacy.as_object_mut().unwrap().remove("syntax_policy");
        std::fs::write(&manifest_path, serde_json::to_vec(&legacy).unwrap()).unwrap();
        let restored = load(dir.path(), view, 1).unwrap();
        assert_eq!(restored.syntax_policy, 0);
        assert!(restored.needs_syntax_refresh());
        assert_eq!(restored.policy, current.policy);
        assert_eq!(restored.tree_hash, current.tree_hash);
        assert_eq!(restored.entries, current.entries);
        assert_eq!(restored.blob_cache(), current.blob_cache());
    }

    #[test]
    fn tree_hash_ignores_skipped_entries() {
        let view = ViewId(uuid::Uuid::nil());
        let policy = ContentHash::of(b"policy");
        let indexed = Entry {
            path: p("a.rs"),
            blob: None,
            state: EntryState::Indexed {
                hash: ContentHash::of(b"a"),
            },
        };
        let skipped = Entry {
            path: p("big.txt"),
            blob: None,
            state: EntryState::Skipped {
                reason: "too_large".into(),
            },
        };
        let with = Manifest::new(view, 1, None, policy, vec![indexed.clone(), skipped]);
        let without = Manifest::new(view, 1, None, policy, vec![indexed]);
        assert_eq!(with.tree_hash, without.tree_hash);
    }
}
