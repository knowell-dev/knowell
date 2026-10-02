//! Source access for Knowell: reading project trees and git objects under
//! the exclusion and secret policies of `knowell-secrets`, and turning
//! repository activity into change signals.
//!
//! - [`fs`] walks a directory (a checkout or any project folder): path
//!   exclusion before a file is opened, size limit before reading, binary
//!   and UTF-8 checks, then secret redaction. Its entry points [`walk`] and
//!   [`read_file`] (one file, same rules) and the report types are
//!   re-exported here.
//! - [`git`] reads git objects directly: resolving a [`TrackTarget`] to a
//!   commit (never substituting another ref), listing and reading a
//!   commit's files (all, a list, or one with
//!   [`git::GitRepo::read_commit_file`]) without touching the user's
//!   checkout, tree diffs with
//!   rename tracking, ancestry checks (force-push detection), worktree
//!   discovery, task-view grouping, and a worktree's uncommitted changes.
//! - [`watch`] is a debounced file-system watcher for one worktree that
//!   reports saved files, `HEAD` moves and ref updates over a
//!   `std::sync::mpsc` channel.
//!
//! The filesystem walker and the git walker share one content pipeline, so
//! a file read from a checkout and the same blob read from git objects
//! produce the same text, findings and hash.
//!
//! [`TrackTarget`]: knowell_core::TrackTarget

mod content;
pub mod fs;
pub mod git;
pub mod watch;

pub use fs::{
    FileRead, SkipReason, SkippedFile, SourceError, SourceFile, WalkOptions, WalkReport, read_file,
    walk,
};
