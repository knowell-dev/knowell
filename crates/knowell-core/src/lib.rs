//! Core domain types shared by every Knowell crate.
//!
//! These types are deliberately small and validated at construction so that
//! the rest of the engine can rely on their invariants:
//!
//! - [`Name`] — workspace / project identifiers (lowercase slugs).
//! - [`RepoPath`] — normalised, repository-relative file paths.
//! - [`ContentHash`] — BLAKE3 content identity.
//! - [`LineRange`] — 1-based inclusive line spans.
//! - [`TrackTarget`] — which ref a project view follows (never guessed).
//! - [`SecretRef`] — a *reference* to a secret; configuration never holds values.

mod hash;
mod name;
mod path;
mod range;
mod schema;
mod secret_ref;
mod track;

pub use hash::{ContentHash, ContentHashError};
pub use name::{Name, NameError};
pub use path::{RepoPath, RepoPathError};
pub use range::{LineRange, LineRangeError};
pub use secret_ref::{SecretRef, SecretRefError};
pub use track::{TrackTarget, TrackTargetError};
